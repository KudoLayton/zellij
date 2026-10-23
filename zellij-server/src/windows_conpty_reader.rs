//! Bounded blocking ConPTY output, adapted from KudoLayton/zellij
//! f98247175, a0187ee79 and ac3fff101 for AsyncReader::read_chunk.
use super::windows_input::InputWriter;
use crate::os_input_output::AsyncReader;
use async_trait::async_trait;
use std::{
    collections::VecDeque,
    io,
    os::windows::io::{AsRawHandle, OwnedHandle},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc::{self, Receiver};
use windows_sys::Win32::{
    Foundation::{ERROR_BROKEN_PIPE, ERROR_NO_DATA, HANDLE},
    Storage::FileSystem::ReadFile,
    System::Pipes::PeekNamedPipe,
};

const READ_SIZE: usize = 65536;
const CHANNEL_CAPACITY: usize = 64;
const DSR_QUERY: &[u8] = b"\x1b[6n";
const DSR_RESPONSE: &[u8] = b"\x1b[1;1R";

pub(super) struct ConPtyBlockingReader {
    rx: Receiver<io::Result<Vec<u8>>>,
    buffered: VecDeque<u8>,
    consumed: Arc<AtomicBool>,
}

impl Drop for ConPtyBlockingReader {
    fn drop(&mut self) {
        self.consumed.store(true, Ordering::Release);
    }
}

impl ConPtyBlockingReader {
    pub(super) fn consumed_signal(&self) -> Arc<AtomicBool> {
        self.consumed.clone()
    }

    /// The worker retains ownership of the read handle until EOF. When the
    /// consumer goes away it continues draining, so ClosePseudoConsole cannot
    /// deadlock trying to emit its final frame into an unread full pipe.
    pub(super) fn new(
        output: OwnedHandle,
        bootstrap_input: Option<Arc<InputWriter>>,
        discard_on_close: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        std::thread::Builder::new()
            .name("zellij-conpty-reader".into())
            .spawn(move || {
                let mut bootstrap = bootstrap_input.map(|input| (DsrBootstrap::new(), input));
                let mut buffer = vec![0; READ_SIZE];
                loop {
                    if let Some((dsr, _)) = bootstrap.as_mut() {
                        if Instant::now() > dsr.deadline {
                            let (tail, _) = dsr.filter(&[], false);
                            send_output(&tx, Ok(tail), &discard_on_close);
                            bootstrap = None;
                        } else {
                            let mut available = 0;
                            let ok = unsafe {
                                PeekNamedPipe(
                                    output.as_raw_handle() as HANDLE,
                                    std::ptr::null_mut(),
                                    0,
                                    std::ptr::null_mut(),
                                    &mut available,
                                    std::ptr::null_mut(),
                                )
                            };
                            if ok != 0 && available == 0 {
                                std::thread::sleep(Duration::from_millis(2));
                                continue;
                            }
                        }
                    }
                    let result = read_output(&output, &mut buffer);
                    let eof = matches!(&result, Ok(0) | Err(_));
                    let mut item = match result {
                        Ok(n) => Ok(buffer[..n].to_vec()),
                        Err(error) => Err(error),
                    };
                    if let Some((dsr, input)) = bootstrap.as_mut() {
                        if let Ok(bytes) = &mut item {
                            let (filtered, answer) = dsr.filter(bytes, eof);
                            *bytes = filtered;
                            if answer {
                                if let Err(error) = input.enqueue_control(DSR_RESPONSE) {
                                    log::debug!(
                                        "Could not queue initial PowerShell DSR response: {error}"
                                    );
                                }
                            }
                        }
                        if item.is_err() {
                            let (tail, _) = dsr.filter(&[], true);
                            send_output(&tx, Ok(tail), &discard_on_close);
                        }
                        if dsr.completed {
                            bootstrap = None;
                        }
                    }
                    send_output(&tx, item, &discard_on_close);
                    if eof {
                        break;
                    }
                }
            })?;
        Ok(Self {
            rx,
            buffered: VecDeque::new(),
            consumed: Arc::new(AtomicBool::new(false)),
        })
    }
}

#[async_trait]
impl AsyncReader for ConPtyBlockingReader {
    async fn read_chunk(&mut self, max: usize) -> io::Result<Vec<u8>> {
        if max == 0 {
            return Ok(Vec::new());
        }
        if self.buffered.is_empty() {
            match self.rx.recv().await {
                Some(Ok(bytes)) => self.buffered.extend(bytes),
                Some(Err(error)) => {
                    self.consumed.store(true, Ordering::Release);
                    return Err(error);
                },
                None => {
                    self.consumed.store(true, Ordering::Release);
                    return Ok(Vec::new());
                },
            }
        }
        let len = max.min(self.buffered.len());
        Ok(self.buffered.drain(..len).collect())
    }
}

fn send_output(
    tx: &mpsc::Sender<io::Result<Vec<u8>>>,
    mut item: io::Result<Vec<u8>>,
    discard: &AtomicBool,
) {
    if matches!(&item, Ok(bytes) if bytes.is_empty()) {
        return;
    }
    // Bounded backpressure is interruptible during explicit teardown.
    while !discard.load(Ordering::Acquire) {
        match tx.try_send(item) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => return,
            Err(mpsc::error::TrySendError::Full(pending)) => {
                item = pending;
                std::thread::sleep(Duration::from_millis(2));
            },
        }
    }
}

fn read_output(handle: &OwnedHandle, buffer: &mut [u8]) -> io::Result<usize> {
    let mut count = 0;
    // SAFETY: the handle is owned by this worker, the buffer is writable and
    // ReadFile is synchronous (the anonymous pipe is not overlapped).
    let ok = unsafe {
        ReadFile(
            handle.as_raw_handle() as HANDLE,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            &mut count,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        return Ok(count as usize);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error().map(|code| code as u32) {
        Some(ERROR_BROKEN_PIPE | ERROR_NO_DATA) => Ok(0),
        _ => Err(error),
    }
}

struct DsrBootstrap {
    deadline: Instant,
    pending: Vec<u8>,
    completed: bool,
}

impl DsrBootstrap {
    fn new() -> Self {
        let millis = std::env::var("ZELLIJ_DSR_BOOTSTRAP_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(200)
            .clamp(50, 2000);
        Self {
            deadline: Instant::now() + Duration::from_millis(millis),
            pending: Vec::new(),
            completed: false,
        }
    }

    fn filter(&mut self, bytes: &[u8], eof: bool) -> (Vec<u8>, bool) {
        let mut combined = std::mem::take(&mut self.pending);
        combined.extend_from_slice(bytes);
        if self.completed || eof || Instant::now() > self.deadline {
            self.completed = true;
            return (combined, false);
        }
        if let Some(index) = combined
            .windows(DSR_QUERY.len())
            .position(|w| w == DSR_QUERY)
        {
            combined.drain(index..index + DSR_QUERY.len());
            self.completed = true;
            return (combined, true);
        }
        let suffix = (1..DSR_QUERY.len())
            .rev()
            .find(|len| combined.ends_with(&DSR_QUERY[..*len]))
            .unwrap_or(0);
        self.pending = combined.split_off(combined.len() - suffix);
        (combined, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Storage::FileSystem::WriteFile;

    fn reader(rx: Receiver<io::Result<Vec<u8>>>) -> ConPtyBlockingReader {
        ConPtyBlockingReader {
            rx,
            buffered: VecDeque::new(),
            consumed: Arc::new(AtomicBool::new(false)),
        }
    }

    #[tokio::test]
    async fn buffers_tail_before_error_and_eof() {
        let (tx, rx) = mpsc::channel(2);
        tx.send(Ok(b"abcdef".to_vec())).await.unwrap();
        tx.send(Err(io::Error::from_raw_os_error(5))).await.unwrap();
        drop(tx);
        let mut reader = reader(rx);
        assert!(reader.read_chunk(0).await.unwrap().is_empty());
        for expected in [b"ab", b"cd", b"ef"] {
            assert_eq!(reader.read_chunk(2).await.unwrap(), expected);
        }
        assert_eq!(
            reader.read_chunk(2).await.unwrap_err().raw_os_error(),
            Some(5)
        );
        assert!(reader.read_chunk(2).await.unwrap().is_empty());
    }

    #[test]
    fn output_queue_applies_backpressure() {
        let (tx, _rx) = mpsc::channel::<io::Result<Vec<u8>>>(CHANNEL_CAPACITY);
        for _ in 0..CHANNEL_CAPACITY {
            tx.try_send(Ok(vec![0; READ_SIZE])).unwrap();
        }
        assert!(matches!(
            tx.try_send(Ok(vec![1])),
            Err(mpsc::error::TrySendError::Full(_))
        ));
    }

    #[tokio::test]
    async fn real_pipe_preserves_bytes_beyond_queue_capacity() {
        let (output, input) = super::super::windows_conpty::pipe_pair().unwrap();
        let mut reader =
            ConPtyBlockingReader::new(output, None, Arc::new(AtomicBool::new(false))).unwrap();
        let expected: Vec<u8> = (0..5 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let sent = expected.clone();
        let writer = std::thread::spawn(move || {
            for chunk in sent.chunks(READ_SIZE) {
                let mut offset = 0;
                while offset < chunk.len() {
                    let mut written = 0;
                    let ok = unsafe {
                        WriteFile(
                            input.as_raw_handle() as HANDLE,
                            chunk[offset..].as_ptr(),
                            (chunk.len() - offset) as u32,
                            &mut written,
                            std::ptr::null_mut(),
                        )
                    };
                    assert_ne!(ok, 0);
                    assert_ne!(written, 0);
                    offset += written as usize;
                }
            }
        });
        let actual = tokio::time::timeout(Duration::from_secs(10), async {
            let mut result = Vec::new();
            loop {
                let bytes = reader.read_chunk(997).await.unwrap();
                if bytes.is_empty() {
                    return result;
                }
                result.extend(bytes);
            }
        })
        .await
        .expect("bounded pipe read deadline");
        writer.join().unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn dropping_consumer_keeps_pipe_draining() {
        let (output, input) = super::super::windows_conpty::pipe_pair().unwrap();
        let reader =
            ConPtyBlockingReader::new(output, None, Arc::new(AtomicBool::new(false))).unwrap();
        drop(reader);
        let (done, completed) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let bytes = vec![1; READ_SIZE];
            for _ in 0..CHANNEL_CAPACITY + 2 {
                let mut written = 0;
                let ok = unsafe {
                    WriteFile(
                        input.as_raw_handle() as HANDLE,
                        bytes.as_ptr(),
                        bytes.len() as u32,
                        &mut written,
                        std::ptr::null_mut(),
                    )
                };
                assert_ne!(ok, 0);
                assert_eq!(written as usize, bytes.len());
            }
            done.send(()).unwrap();
        });
        completed
            .recv_timeout(Duration::from_secs(5))
            .expect("abandoned reader must drain output");
    }

    #[test]
    fn bootstrap_handles_every_query_split_once() {
        for at in 0..=DSR_QUERY.len() {
            let mut dsr = DsrBootstrap::new();
            let (a, first) = dsr.filter(&DSR_QUERY[..at], false);
            let (b, second) = dsr.filter(&DSR_QUERY[at..], false);
            assert!(a.is_empty() && b.is_empty());
            assert_ne!(first, second);
            assert_eq!(dsr.filter(DSR_QUERY, false), (DSR_QUERY.to_vec(), false));
        }
    }

    #[test]
    fn bootstrap_preserves_false_prefix_and_surrounding_output() {
        let mut dsr = DsrBootstrap::new();
        assert_eq!(
            dsr.filter(b"before\x1b[", false),
            (b"before".to_vec(), false)
        );
        assert_eq!(
            dsr.filter(b"XXafter", false),
            (b"\x1b[XXafter".to_vec(), false)
        );
        assert_eq!(
            dsr.filter(b"before\x1b[6nafter", false),
            (b"beforeafter".to_vec(), true)
        );
    }

    #[test]
    fn bootstrap_preserves_pending_on_eof_and_expiry() {
        for eof in [true, false] {
            let mut dsr = DsrBootstrap::new();
            dsr.filter(b"\x1b[", false);
            if !eof {
                dsr.deadline = Instant::now() - Duration::from_secs(1);
            }
            assert_eq!(dsr.filter(b"", eof), (b"\x1b[".to_vec(), false));
        }
    }
}
