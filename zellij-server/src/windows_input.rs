//! Pane-local, bounded ConPTY input worker. Cancellation/completion handling
//! adapted from Helvesec/rmux 15365111 (bounded_write.rs at 1f4571e7).
//! Copyright (c) 2026 The RMUX Authors. MIT: see windows_input.LICENSE.
//!
//! Unlike rmux, recovery keeps the SAME OVERLAPPED request alive. A cancelled
//! byte-pipe write may have exposed a prefix: never resubmit ambiguous bytes.
use crossbeam::channel::{self, Receiver, Sender, TryRecvError, TrySendError};
use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_IO_PENDING, ERROR_NOT_FOUND, ERROR_OPERATION_ABORTED, HANDLE, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    },
    Storage::FileSystem::WriteFile,
    System::{
        Threading::{CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects},
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
    },
};

const CHUNK_SIZE: usize = 4096;
const QUEUE_CAPACITY: usize = 64;

#[derive(Clone, Copy)]
struct Policy {
    primary: Duration,
    recovery: Duration,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            primary: Duration::from_secs(2),
            recovery: Duration::from_millis(500),
        }
    }
}
#[derive(Clone)]
struct Failure {
    kind: io::ErrorKind,
    message: String,
}
impl Failure {
    fn from_error(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
    fn error(&self) -> io::Error {
        io::Error::new(self.kind, self.message.clone())
    }
}
struct State {
    stopped: AtomicBool,
    stalled: AtomicBool,
    error: Mutex<Option<Failure>>,
    stop_event: OwnedHandle,
}
impl State {
    fn error(&self) -> Option<io::Error> {
        self.error.lock().unwrap().as_ref().map(Failure::error)
    }
    fn fail(&self, error: &io::Error) {
        let mut saved = self.error.lock().unwrap();
        if saved.is_none() {
            *saved = Some(Failure::from_error(error));
        }
    }
}
struct Pending {
    bytes: Arc<[u8]>,
    reply: Receiver<io::Result<usize>>,
}
enum Message {
    Write(Arc<[u8]>, Option<Sender<io::Result<usize>>>),
    Barrier(Sender<io::Result<usize>>),
}

pub(super) struct InputWriter {
    tx: Sender<Message>,
    stop: Sender<()>,
    state: Arc<State>,
    pending: Mutex<Option<Pending>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    policy: Policy,
}
impl InputWriter {
    #[cfg(test)]
    pub(super) fn is_stalled(&self) -> bool {
        self.state.stalled.load(Ordering::Acquire)
    }

    pub(super) fn new(handle: OwnedHandle) -> io::Result<Self> {
        Self::with_policy(handle, Policy::default())
    }

    fn with_policy(handle: OwnedHandle, policy: Policy) -> io::Result<Self> {
        let state = Arc::new(State {
            stopped: AtomicBool::new(false),
            stalled: AtomicBool::new(false),
            error: Mutex::new(None),
            stop_event: new_event()?,
        });
        let mut writer = PipeWriter {
            handle,
            event: new_event()?,
            state: state.clone(),
            policy,
        };
        let (tx, rx) = channel::bounded(QUEUE_CAPACITY);
        let (stop, stopped) = channel::bounded(1);
        let worker_state = state.clone();
        let worker = std::thread::Builder::new().name("zellij-conpty-input".into()).spawn(move || {
            loop {
                let message = channel::select_biased! {
                    recv(stopped) -> _ => break,
                    recv(rx) -> message => match message { Ok(message) => message, Err(_) => break },
                };
                if worker_state.stopped.load(Ordering::Acquire) { break; }
                match message {
                    Message::Write(bytes, reply) => {
                        let (written, error) = writer.write(&bytes);
                        if let Some(error) = &error {
                            worker_state.fail(error);
                            if !worker_state.stopped.load(Ordering::Acquire) {
                                log::error!("ConPTY input stopped after {written}/{} confirmed bytes: {error}", bytes.len());
                            }
                        }
                        if let Some(reply) = reply {
                            let result = if written > 0 { Ok(written) }
                                else if let Some(error) = &error { Err(Failure::from_error(error).error()) }
                                else { Ok(0) };
                            let _ = reply.send(result);
                        }
                        if error.is_some() { break; }
                    }
                    Message::Barrier(reply) => { let _ = reply.send(Ok(0)); }
                }
            }
            worker_state.stopped.store(true, Ordering::Release);
            // Dropping rx disconnects all pending replies. Owned I/O handles are
            // released only after PipeWriter has drained its outstanding request.
        })?;
        Ok(Self {
            tx,
            stop,
            state,
            pending: Mutex::new(None),
            worker: Mutex::new(Some(worker)),
            policy,
        })
    }

    /// Matches the existing pty_writer scheduler: Ok(0) means retry later.
    /// Only acknowledge bytes after confirmed OS completion, not queue admission.
    pub(super) fn try_write(&self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut pending = self.pending.lock().unwrap();
        if let Some(request) = pending.as_ref() {
            if !bytes.starts_with(&request.bytes) {
                let error = io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "ConPTY input changed while a write was outstanding; refusing ambiguous replay",
                );
                self.state.fail(&error);
                self.request_stop();
                return Err(error);
            }
            match request.reply.try_recv() {
                Ok(result) => {
                    *pending = None;
                    return result;
                },
                Err(TryRecvError::Empty) => return Ok(0),
                Err(TryRecvError::Disconnected) => {
                    *pending = None;
                    return Err(self.failure());
                },
            }
        }
        if let Some(error) = self.state.error() {
            return Err(error);
        }
        if self.state.stopped.load(Ordering::Acquire) {
            return Err(self.failure());
        }
        let bytes: Arc<[u8]> = Arc::from(&bytes[..bytes.len().min(CHUNK_SIZE)]);
        let (tx, rx) = channel::bounded(1);
        match self.tx.try_send(Message::Write(bytes.clone(), Some(tx))) {
            Ok(()) => {
                *pending = Some(Pending { bytes, reply: rx });
                Ok(0)
            },
            Err(TrySendError::Full(_)) => Ok(0),
            Err(TrySendError::Disconnected(_)) => Err(self.failure()),
        }
    }

    /// Small terminal-protocol replies and SIGINT use the same FIFO as input.
    /// Never wait in the output reader (which must keep draining during stalls).
    pub(super) fn enqueue_control(&self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if bytes.len() > CHUNK_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "oversized ConPTY control write",
            ));
        }
        if let Some(error) = self.state.error() {
            return Err(error);
        }
        if self.state.stopped.load(Ordering::Acquire) {
            return Err(self.failure());
        }
        self.tx
            .try_send(Message::Write(Arc::from(bytes), None))
            .map_err(|error| match error {
                TrySendError::Full(_) => {
                    io::Error::new(io::ErrorKind::WouldBlock, "ConPTY control queue is full")
                },
                TrySendError::Disconnected(_) => self.failure(),
            })
    }

    /// Ordered barrier for admitted writes. Do not call FlushFileBuffers on a
    /// pipe: it waits for the reader and cannot be cancelled like overlapped I/O.
    pub(super) fn drain(&self) -> io::Result<()> {
        if let Some(error) = self.state.error() {
            return Err(error);
        }
        if self.state.stopped.load(Ordering::Acquire) {
            return Err(self.failure());
        }
        let (tx, rx) = channel::bounded(1);
        self.tx.try_send(Message::Barrier(tx)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "ConPTY barrier queue unavailable",
            )
        })?;
        match rx
            .recv_timeout(self.policy.primary + self.policy.recovery + Duration::from_millis(100))
        {
            Ok(result) => result.map(|_| ()),
            Err(channel::RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "ConPTY drain deadline exceeded",
            )),
            Err(channel::RecvTimeoutError::Disconnected) => Err(self.failure()),
        }
    }

    fn failure(&self) -> io::Error {
        self.state.error().unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "ConPTY input worker stopped")
        })
    }
    fn request_stop(&self) {
        self.state.stopped.store(true, Ordering::Release);
        unsafe {
            SetEvent(self.state.stop_event.as_raw_handle() as HANDLE);
        }
        let _ = self.stop.try_send(());
    }
    pub(super) fn shutdown(&self) {
        self.request_stop();
        // Keep this lock through join so concurrent shutdown callers all wait
        // for cancellation to finish. No map/input-queue mutex is held here.
        let mut worker = self.worker.lock().unwrap();
        if let Some(worker) = worker.take() {
            if worker.join().is_err() {
                log::error!("ConPTY input worker panicked");
            }
        }
    }
}
impl Drop for InputWriter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn new_event() -> io::Result<OwnedHandle> {
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if event.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(event) })
    }
}

struct PipeWriter {
    handle: OwnedHandle,
    event: OwnedHandle,
    state: Arc<State>,
    policy: Policy,
}
impl PipeWriter {
    fn write(&mut self, bytes: &[u8]) -> (usize, Option<io::Error>) {
        let mut offset = 0;
        while offset < bytes.len() {
            if self.state.stopped.load(Ordering::Acquire) {
                return (
                    offset,
                    Some(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "ConPTY input cancelled",
                    )),
                );
            }
            match self.chunk(&bytes[offset..]) {
                Ok(0) => {
                    return (
                        offset,
                        Some(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "ConPTY input accepted zero bytes",
                        )),
                    )
                },
                Ok(n) => {
                    offset += n;
                    self.state.stalled.store(false, Ordering::Release);
                },
                Err(error) => return (offset, Some(error)),
            }
        }
        (offset, None)
    }

    fn chunk(&self, bytes: &[u8]) -> io::Result<usize> {
        if unsafe { ResetEvent(self.event.as_raw_handle() as HANDLE) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = self.event.as_raw_handle() as HANDLE;
        // SAFETY: bytes and OVERLAPPED remain alive through completion or
        // cancel_and_drain on EVERY pending exit path below.
        let started = unsafe {
            WriteFile(
                self.handle.as_raw_handle() as HANDLE,
                bytes.as_ptr(),
                bytes.len() as u32,
                std::ptr::null_mut(),
                &mut overlapped,
            )
        };
        if started != 0 {
            return self.completed(&overlapped, false);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
            return Err(error);
        }
        let events = [
            self.event.as_raw_handle() as HANDLE,
            self.state.stop_event.as_raw_handle() as HANDLE,
        ];
        let mut recovering = false;
        loop {
            let timeout = if recovering {
                self.policy.recovery
            } else {
                self.policy.primary
            };
            let status =
                unsafe { WaitForMultipleObjects(2, events.as_ptr(), 0, timeout_ms(timeout)) };
            if status == WAIT_OBJECT_0 {
                return self.completed(&overlapped, false);
            }
            if status == WAIT_TIMEOUT && !recovering && !self.policy.recovery.is_zero() {
                recovering = true;
                self.state.stalled.store(true, Ordering::Release);
                continue; // preserve the same pending I/O; do not cancel/replay it
            }
            let error = if status == WAIT_TIMEOUT {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "ConPTY input made no progress through recovery grace",
                )
            } else if status == WAIT_OBJECT_0 + 1 {
                io::Error::new(io::ErrorKind::Interrupted, "ConPTY input cancelled")
            } else {
                io::Error::last_os_error()
            };
            // Completion can win the cancellation race: acknowledge those bytes.
            return match self.cancel_and_drain(&overlapped)? {
                Some(count) => Ok(count),
                None => Err(error),
            };
        }
    }

    fn completed(&self, overlapped: &OVERLAPPED, wait: bool) -> io::Result<usize> {
        let mut transferred = 0;
        if unsafe {
            GetOverlappedResult(
                self.handle.as_raw_handle() as HANDLE,
                overlapped,
                &mut transferred,
                i32::from(wait),
            )
        } == 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(transferred as usize)
        }
    }

    fn cancel_and_drain(&self, overlapped: &OVERLAPPED) -> io::Result<Option<usize>> {
        let cancelled = unsafe { CancelIoEx(self.handle.as_raw_handle() as HANDLE, overlapped) };
        let cancellation_error = if cancelled == 0 {
            let error = io::Error::last_os_error();
            (error.raw_os_error() != Some(ERROR_NOT_FOUND as i32)).then_some(error)
        } else {
            None
        };
        // Cancellation is a request, not completion. Never free the buffer or
        // OVERLAPPED before this wait, even if CancelIoEx itself failed.
        match self.completed(overlapped, true) {
            Ok(count) => Ok(Some(count)),
            Err(error) if error.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32) => {
                if let Some(error) = cancellation_error {
                    Err(error)
                } else {
                    Ok(None)
                }
            },
            Err(error) => Err(error),
        }
    }
}

fn timeout_ms(duration: Duration) -> u32 {
    duration
        .as_millis()
        .saturating_add(u128::from(duration.subsec_nanos() % 1_000_000 != 0))
        .min((u32::MAX - 1) as u128) as u32
}

#[cfg(test)]
#[path = "windows_input_tests.rs"]
mod tests;
