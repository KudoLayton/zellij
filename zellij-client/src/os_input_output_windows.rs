use crate::os_input_output::SignalEvent;
use crate::stdin_handler_windows::restore_vt_input;

use anyhow::{Context, Result};
use async_trait::async_trait;

use std::io;
use std::io::Write;
use std::path::Path;
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetStdHandle, WriteConsoleW, STD_OUTPUT_HANDLE,
};
use zellij_utils::ipc::{IpcReceiverWithContext, IpcSenderWithContext};

/// Windows async signal listener.
///
/// Polls `crossterm::terminal::size()` at 100ms intervals for resize events,
/// and listens to `tokio::signal::windows` for ctrl_c/ctrl_break/ctrl_close.
pub(crate) struct AsyncSignalListener {
    interval: tokio::time::Interval,
    last_size: (u16, u16),
    ctrl_c: tokio::signal::windows::CtrlC,
    ctrl_break: tokio::signal::windows::CtrlBreak,
    ctrl_close: tokio::signal::windows::CtrlClose,
}

impl AsyncSignalListener {
    pub fn new() -> io::Result<Self> {
        let size = crossterm::terminal::size().unwrap_or((80, 24));
        Ok(Self {
            interval: tokio::time::interval(std::time::Duration::from_millis(100)),
            last_size: size,
            ctrl_c: tokio::signal::windows::ctrl_c()?,
            ctrl_break: tokio::signal::windows::ctrl_break()?,
            ctrl_close: tokio::signal::windows::ctrl_close()?,
        })
    }
}

#[async_trait]
impl crate::os_input_output::AsyncSignals for AsyncSignalListener {
    async fn recv(&mut self) -> Option<SignalEvent> {
        loop {
            tokio::select! {
                _ = self.interval.tick() => {
                    if let Ok(new_size) = crossterm::terminal::size() {
                        if new_size != self.last_size {
                            self.last_size = new_size;
                            return Some(SignalEvent::Resize);
                        }
                    }
                }
                result = self.ctrl_c.recv() => {
                    return result.map(|_| SignalEvent::Quit);
                }
                result = self.ctrl_break.recv() => {
                    return result.map(|_| SignalEvent::Quit);
                }
                result = self.ctrl_close.recv() => {
                    return result.map(|_| SignalEvent::Quit);
                }
            }
        }
    }
}

/// Windows blocking signal iterator.
///
/// Uses `SetConsoleCtrlHandler` with an `AtomicBool` for quit signals.
/// For resize detection, operates in two modes:
/// - **Channel mode**: receives resize notifications forwarded from the stdin
///   thread (which gets `Event::Resize` from crossterm). Much more responsive
///   than polling.
/// - **Poll fallback**: polls `crossterm::terminal::size()` at 50ms intervals.
///   Used when no receiver is provided or when the sender is dropped (VT reader
///   path).
pub(crate) struct BlockingSignalIterator {
    last_size: (u16, u16),
    resize_receiver: Option<std::sync::mpsc::Receiver<()>>,
}

mod win_ctrl_handler {
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT};

    pub static CTRL_QUIT_RECEIVED: AtomicBool = AtomicBool::new(false);

    pub unsafe extern "system" fn ctrl_handler(ctrl_type: u32) -> BOOL {
        match ctrl_type {
            CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT => {
                CTRL_QUIT_RECEIVED.store(true, Ordering::SeqCst);
                1 // TRUE — handled
            },
            _ => 0, // FALSE — not handled
        }
    }
}

impl BlockingSignalIterator {
    pub fn new(resize_receiver: Option<std::sync::mpsc::Receiver<()>>) -> io::Result<Self> {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

        win_ctrl_handler::CTRL_QUIT_RECEIVED.store(false, std::sync::atomic::Ordering::SeqCst);

        let ok = unsafe { SetConsoleCtrlHandler(Some(win_ctrl_handler::ctrl_handler), 1) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        let size = crossterm::terminal::size().unwrap_or((80, 24));
        Ok(Self {
            last_size: size,
            resize_receiver,
        })
    }
}

impl Iterator for BlockingSignalIterator {
    type Item = SignalEvent;

    fn next(&mut self) -> Option<SignalEvent> {
        use std::sync::mpsc::RecvTimeoutError;
        use std::time::Duration;

        loop {
            if win_ctrl_handler::CTRL_QUIT_RECEIVED.load(std::sync::atomic::Ordering::SeqCst) {
                return Some(SignalEvent::Quit);
            }

            if let Some(ref rx) = self.resize_receiver {
                // Channel mode: the native console stdin loop sends resize
                // notifications through this channel. Block with a timeout so
                // we can periodically check the quit flag above.
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(()) => return Some(SignalEvent::Resize),
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => {
                        // Sender dropped (VT reader path) — switch to poll mode.
                        self.resize_receiver = None;
                        continue;
                    },
                }
            } else {
                // Poll mode: used on the VT reader path where crossterm's
                // event::read() isn't used, so resize events don't come through
                // the channel. Periodically compare the terminal size instead.
                if let Ok(new_size) = crossterm::terminal::size() {
                    if new_size != self.last_size {
                        self.last_size = new_size;
                        return Some(SignalEvent::Resize);
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Set up client IPC channels from a connected socket.
///
/// On Windows we use two separate named pipes to avoid DuplicateHandle
/// deadlock: the command pipe (socket) for client→server, and a reply pipe
/// for server→client.
pub(crate) fn setup_ipc(
    socket: interprocess::local_socket::Stream,
    path: &Path,
) -> (
    IpcSenderWithContext<zellij_utils::ipc::ClientToServerMsg>,
    IpcReceiverWithContext<zellij_utils::ipc::ServerToClientMsg>,
) {
    let reply_socket;
    loop {
        match zellij_utils::consts::ipc_connect_reply(path) {
            Ok(sock) => {
                reply_socket = sock;
                break;
            },
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            },
        }
    }
    let sender = IpcSenderWithContext::new(socket);
    let receiver = IpcReceiverWithContext::new(reply_socket);
    (sender, receiver)
}

/// Enable ENABLE_VIRTUAL_TERMINAL_PROCESSING on stdout so that ConPTY enters
/// passthrough mode and forwards DEC private mode sequences (like mouse-enable)
/// to the terminal emulator.  Uses crossterm's safe wrapper which handles the
/// GetConsoleMode/SetConsoleMode internally.
fn enable_vt_processing_on_stdout() {
    crossterm::ansi_support::supports_ansi();
}

/// Enable mouse support on Windows.
///
/// When TERM is set we're on the VT input path (terminal emulator like
/// Alacritty via ConPTY). We must NOT use crossterm's EnableMouseCapture
/// because it does a full SetConsoleMode() that would overwrite the mode
/// set by enable_vt_input(), clobbering ENABLE_VIRTUAL_TERMINAL_INPUT.
///
/// Instead, we enable ENABLE_VIRTUAL_TERMINAL_PROCESSING on stdout so
/// ConPTY enters passthrough mode, then write ANSI mouse-enable sequences.
///
/// When TERM is not set we're in a native console (cmd, PowerShell,
/// Windows Terminal) and use crossterm's Console API approach.
pub(crate) fn enable_mouse_support(stdout: &mut dyn Write) -> Result<()> {
    let err_context = "failed to enable mouse mode";
    if std::env::var("TERM").is_ok() {
        enable_vt_processing_on_stdout();
        stdout
            .write_all(super::os_input_output::ENABLE_MOUSE_SUPPORT.as_bytes())
            .context(err_context)?;
        stdout.flush().context(err_context)?;
    } else {
        // crossterm::execute! requires Sized, so we use std::io::stdout()
        // directly rather than the trait-object writer.
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)
            .context(err_context)?;
    }
    Ok(())
}

/// Restore the console input mode to its pre-Zellij state.
///
/// On the VT path, `enable_vt_input()` sets ENABLE_MOUSE_INPUT and
/// ENABLE_VIRTUAL_TERMINAL_INPUT on the console handle, but crossterm's
/// `disable_raw_mode()` never clears them.  This function restores the
/// original console mode saved before those flags were set.
pub(crate) fn restore_console_mode() {
    restore_vt_input();
}

/// Disable mouse support on Windows.
///
/// See `enable_mouse_support()` for rationale on VT vs Console API paths.
pub(crate) fn disable_mouse_support(stdout: &mut dyn Write) -> Result<()> {
    let err_context = "failed to disable mouse mode";
    if std::env::var("TERM").is_ok() {
        stdout
            .write_all(super::os_input_output::DISABLE_MOUSE_SUPPORT.as_bytes())
            .context(err_context)?;
        stdout.flush().context(err_context)?;
    } else {
        crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture)
            .context(err_context)?;
    }
    Ok(())
}

pub(crate) fn stdout_writer() -> Box<dyn Write> {
    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Box::new(std::io::stdout());
    }
    let mut mode = 0;
    let is_console = unsafe { GetConsoleMode(handle, &mut mode) } != 0;
    if is_console {
        Box::new(WindowsConsoleWriter::new(handle))
    } else {
        Box::new(std::io::stdout())
    }
}

struct WindowsConsoleWriter {
    handle: HANDLE,
    pending_utf8: Vec<u8>,
}

unsafe impl Send for WindowsConsoleWriter {}

impl WindowsConsoleWriter {
    fn new(handle: HANDLE) -> Self {
        Self {
            handle,
            pending_utf8: Vec::new(),
        }
    }

    fn write_utf16(&mut self, text: &str) -> io::Result<()> {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        let mut offset = 0;
        while offset < utf16.len() {
            let chunk = &utf16[offset..];
            let mut written = 0;
            let ok = unsafe {
                WriteConsoleW(
                    self.handle,
                    chunk.as_ptr().cast(),
                    chunk.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            offset += written as usize;
        }
        Ok(())
    }
}

impl Write for WindowsConsoleWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending_utf8.extend_from_slice(buf);
        let pending = std::mem::take(&mut self.pending_utf8);
        let split = split_valid_utf8_prefix(&pending);
        if !split.valid.is_empty() {
            let text = std::str::from_utf8(split.valid)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            self.write_utf16(text)?;
        }
        self.pending_utf8.extend_from_slice(split.pending);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Utf8PrefixSplit<'a> {
    valid: &'a [u8],
    pending: &'a [u8],
}

fn split_valid_utf8_prefix(bytes: &[u8]) -> Utf8PrefixSplit<'_> {
    match std::str::from_utf8(bytes) {
        Ok(_) => Utf8PrefixSplit {
            valid: bytes,
            pending: &[],
        },
        Err(e) if e.error_len().is_none() => {
            let valid_up_to = e.valid_up_to();
            Utf8PrefixSplit {
                valid: &bytes[..valid_up_to],
                pending: &bytes[valid_up_to..],
            }
        },
        Err(_) => Utf8PrefixSplit {
            valid: bytes,
            pending: &[],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_prefix_split_keeps_incomplete_codepoint_pending() {
        let bytes = "a한".as_bytes();
        let split = split_valid_utf8_prefix(&bytes[..bytes.len() - 1]);

        assert_eq!(split.valid, b"a");
        assert_eq!(split.pending, &bytes[1..bytes.len() - 1]);
    }

    #[test]
    fn utf8_prefix_split_accepts_complete_codepoints() {
        let bytes = "a한".as_bytes();
        let split = split_valid_utf8_prefix(bytes);

        assert_eq!(split.valid, bytes);
        assert!(split.pending.is_empty());
    }

    #[test]
    fn console_writer_flush_keeps_split_codepoint_pending() {
        let glyph = "é".as_bytes();
        let mut writer = WindowsConsoleWriter {
            handle: std::ptr::null_mut(),
            pending_utf8: glyph[..1].to_vec(),
        };

        writer.flush_pending().expect("split utf8 waits");

        assert_eq!(writer.pending_utf8, glyph[..1]);
    }
}
