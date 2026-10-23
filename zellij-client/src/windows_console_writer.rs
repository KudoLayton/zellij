//! Console output adapted from KudoLayton/zellij dacff8f3a and 52270448f.
//! Preserve incomplete UTF-8 across writes and flushes; redirected stdout stays byte-oriented.
use std::io::{self, Write};
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetStdHandle, WriteConsoleW, STD_OUTPUT_HANDLE,
};

pub(crate) fn stdout_writer() -> Box<dyn Write> {
    // SAFETY: the process owns its standard handle; this writer only borrows it.
    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Box::new(io::stdout());
    }
    let mut mode = 0;
    // SAFETY: mode is writable and handle was checked above.
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        Box::new(io::stdout())
    } else {
        Box::new(WindowsConsoleWriter {
            handle,
            pending_utf8: Vec::new(),
        })
    }
}

struct WindowsConsoleWriter {
    handle: HANDLE,
    pending_utf8: Vec<u8>,
}

impl WindowsConsoleWriter {
    fn write_utf16(&mut self, text: &str) -> io::Result<()> {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        let mut offset = 0;
        while offset < utf16.len() {
            let chunk = &utf16[offset..];
            let mut written = 0;
            // SAFETY: the slice remains alive throughout the synchronous call;
            // written is writable and the standard handle is not closed here.
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
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "WriteConsoleW wrote zero UTF-16 code units",
                ));
            }
            offset += written as usize;
        }
        Ok(())
    }

    fn write_pending_valid_prefix(&mut self) -> io::Result<()> {
        let pending = std::mem::take(&mut self.pending_utf8);
        let (valid, tail) = split_valid_utf8_prefix(&pending);
        if !valid.is_empty() {
            self.write_utf16(&String::from_utf8_lossy(valid))?;
        }
        self.pending_utf8.extend_from_slice(tail);
        Ok(())
    }
}

impl Write for WindowsConsoleWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending_utf8.extend_from_slice(buf);
        self.write_pending_valid_prefix()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.write_pending_valid_prefix()
    }
}

fn split_valid_utf8_prefix(bytes: &[u8]) -> (&[u8], &[u8]) {
    let mut offset = 0;
    while offset < bytes.len() {
        match std::str::from_utf8(&bytes[offset..]) {
            Ok(_) => break,
            Err(error) => {
                offset += error.valid_up_to();
                match error.error_len() {
                    Some(invalid_len) => offset += invalid_len,
                    None => return bytes.split_at(offset),
                }
            },
        }
    }
    (bytes, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_prefix_split_keeps_incomplete_codepoint_pending() {
        let bytes = "a한".as_bytes();
        let (valid, pending) = split_valid_utf8_prefix(&bytes[..bytes.len() - 1]);
        assert_eq!(valid, b"a");
        assert_eq!(pending, &bytes[1..bytes.len() - 1]);
    }

    #[test]
    fn utf8_prefix_split_accepts_complete_codepoints() {
        let bytes = "a한".as_bytes();
        let (valid, pending) = split_valid_utf8_prefix(bytes);
        assert_eq!(valid, bytes);
        assert!(pending.is_empty());
    }

    #[test]
    fn console_writer_flush_keeps_split_codepoint_pending() {
        let glyph = "é".as_bytes();
        let mut writer = WindowsConsoleWriter {
            handle: std::ptr::null_mut(),
            pending_utf8: glyph[..1].to_vec(),
        };
        writer
            .flush()
            .expect("split utf8 waits without touching console");
        writer.flush().expect("repeated flush also waits");
        assert_eq!(writer.pending_utf8, glyph[..1]);
    }

    #[test]
    fn invalid_prefix_does_not_consume_a_later_split_unicode_character() {
        for text in ["한", "😀"] {
            for boundary in 1..text.len() {
                let bytes = [b"\xff".as_slice(), &text.as_bytes()[..boundary]].concat();
                let (prefix, tail) = split_valid_utf8_prefix(&bytes);
                assert_eq!(prefix, b"\xff");
                assert_eq!(tail, &text.as_bytes()[..boundary]);
                let complete = [tail, &text.as_bytes()[boundary..]].concat();
                assert_eq!(std::str::from_utf8(&complete).unwrap(), text);
            }
        }
    }

    #[test]
    fn unicode_boundaries_preserve_every_incomplete_suffix() {
        for text in ["한", "😀", "a한😀"] {
            for boundary in 0..=text.len() {
                let prefix = &text.as_bytes()[..boundary];
                let (valid, pending) = split_valid_utf8_prefix(prefix);
                assert!(std::str::from_utf8(valid).is_ok());
                assert_eq!([valid, pending].concat(), prefix);
                assert_eq!(pending.is_empty(), text.is_char_boundary(boundary));
            }
        }
    }
}
