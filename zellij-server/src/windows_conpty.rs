//! ConPTY allocation and fallback policy from KudoLayton/zellij b56a18ff3
//! and e3ed549f2. Each failed spawn gets a fresh pseudo console and pipes.
use super::{
    windows_conpty_reader::ConPtyBlockingReader, windows_input::InputWriter,
    windows_input_pipe::input_pipe,
};
use std::{
    io,
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use windows_sys::Win32::{
    Foundation::{HANDLE, S_OK},
    System::{
        Console::{ClosePseudoConsole, CreatePseudoConsole, COORD, HPCON},
        Pipes::CreatePipe,
        SystemInformation::OSVERSIONINFOEXW,
    },
};

pub(super) const PASSTHROUGH: u32 = 8;
const BASE_FLAGS: u32 = 2 | 4;

pub(super) fn select_flags(build: u32, disabled: bool) -> u32 {
    BASE_FLAGS
        | if build >= 22621 && !disabled {
            PASSTHROUGH
        } else {
            0
        }
}

pub(super) fn fallback_flags(flags: u32) -> Option<u32> {
    if flags & PASSTHROUGH != 0 {
        Some(BASE_FLAGS)
    } else if flags != 0 {
        Some(0)
    } else {
        None
    }
}

pub(super) fn selected_flags() -> u32 {
    let disabled = std::env::var("ZELLIJ_CONPTY_NO_PASSTHROUGH")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);
    let mut info: OSVERSIONINFOEXW = unsafe { std::mem::zeroed() };
    info.dwOSVersionInfoSize = size_of::<OSVERSIONINFOEXW>() as u32;
    let status = unsafe { RtlGetVersion(&mut info) };
    select_flags(if status >= 0 { info.dwBuildNumber } else { 0 }, disabled)
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(info: *mut OSVERSIONINFOEXW) -> i32;
}

pub(super) fn pipe_pair() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read = std::ptr::null_mut();
    let mut write = std::ptr::null_mut();
    // Null SECURITY_ATTRIBUTES: handles are deliberately not inheritable.
    if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        )
    })
}

pub(super) struct Console {
    pub(super) hpcon: HPCON,
    pub(super) input: Arc<InputWriter>,
    pub(super) flags: u32,
    discard_on_close: Arc<AtomicBool>,
}

// The handle is owned, and backend serializes resize/close operations.
unsafe impl Send for Console {}
unsafe impl Sync for Console {}

impl Console {
    pub(super) fn create(
        cols: u16,
        rows: u16,
        mut flags: u32,
        bootstrap: bool,
    ) -> io::Result<(Self, ConPtyBlockingReader)> {
        loop {
            let (input_read, input_handle) = input_pipe(4096)?;
            let input = Arc::new(InputWriter::new(input_handle)?);
            let (output, output_write) = pipe_pair()?;
            let discard_on_close = Arc::new(AtomicBool::new(false));
            let bootstrap_input = if bootstrap { Some(input.clone()) } else { None };
            // Start draining before CreatePseudoConsole / any subsequent close.
            let reader =
                ConPtyBlockingReader::new(output, bootstrap_input, discard_on_close.clone())?;
            let size = COORD {
                X: cols.max(1).min(i16::MAX as u16) as i16,
                Y: rows.max(1).min(i16::MAX as u16) as i16,
            };
            let mut hpcon = 0;
            let hr = unsafe {
                CreatePseudoConsole(
                    size,
                    input_read.as_raw_handle() as HANDLE,
                    output_write.as_raw_handle() as HANDLE,
                    flags,
                    &mut hpcon,
                )
            };
            if hr == S_OK {
                return Ok((
                    Self {
                        hpcon,
                        input,
                        flags,
                        discard_on_close,
                    },
                    reader,
                ));
            }
            drop(reader);
            match fallback_flags(flags) {
                Some(next) => {
                    log::debug!(
                        "CreatePseudoConsole flags {flags:#x} failed ({hr:#x}); retry {next:#x}"
                    );
                    flags = next;
                },
                None => return Err(io::Error::from_raw_os_error(hr)),
            }
        }
    }

    pub(super) fn discard_signal(&self) -> Arc<AtomicBool> {
        self.discard_on_close.clone()
    }

    /// Natural exit keeps the reader delivering its final frame. Explicit
    /// teardown uses Drop instead, releasing bounded backpressure first.
    pub(super) fn close_after_exit(&mut self) {
        self.input.shutdown();
        if self.hpcon != 0 {
            let hpcon = std::mem::replace(&mut self.hpcon, 0);
            unsafe { ClosePseudoConsole(hpcon) };
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.discard_on_close.store(true, Ordering::Release);
        self.close_after_exit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn flags_respect_version_and_override() {
        assert_eq!(select_flags(22621, false), 14);
        assert_eq!(select_flags(19045, false), BASE_FLAGS);
        assert_eq!(select_flags(22621, true), BASE_FLAGS);
        assert_eq!(select_flags(0, false), BASE_FLAGS);
    }
    #[test]
    fn fallback_is_finite_and_removes_passthrough_first() {
        assert_eq!(fallback_flags(14), Some(6));
        assert_eq!(fallback_flags(6), Some(0));
        assert_eq!(fallback_flags(0), None);
    }
}
