use std::sync::OnceLock;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_EXTENDED_FLAGS, ENABLE_MOUSE_INPUT,
    ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_WINDOW_INPUT, STD_INPUT_HANDLE,
};

use crate::keyboard_parser::{KittyKeyboardParser, KittyParseOutcome};
use crate::InputInstruction;
use zellij_utils::channels::SenderWithContext;
use zellij_utils::data::{BareKey, KeyWithModifier};
use zellij_utils::input::{cast_crossterm_key, from_crossterm_mouse};
use zellij_utils::input_trace;
use zellij_utils::vendored::termwiz::input::InputEvent;

const NATIVE_KITTY_SEQUENCE_TIMEOUT: Duration = Duration::from_millis(30);
const NATIVE_KITTY_SEQUENCE_MAX_LEN: usize = 32;

/// Saved console input mode from before `enable_vt_input()` modified it.
/// Used by `restore_vt_input()` to put the console back the way the shell
/// left it, clearing flags like ENABLE_MOUSE_INPUT that crossterm's
/// disable_raw_mode() does not touch.
static ORIGINAL_CONSOLE_MODE: OnceLock<u32> = OnceLock::new();

/// Set the stdin console mode for raw VT input.
///
/// Instead of just ORing in ENABLE_VIRTUAL_TERMINAL_INPUT on top of whatever
/// the current mode happens to be, we explicitly set the exact mode we need.
/// This avoids a TOCTOU race with crossterm's EnableMouseCapture (which also
/// does GetConsoleMode/SetConsoleMode) and ensures flags like
/// ENABLE_QUICK_EDIT_MODE are always cleared — that flag intercepts mouse
/// events at the console level, breaking application mouse support.
pub(crate) fn enable_vt_input() -> bool {
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut mode: u32 = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return false;
        }
        // Save the original mode so we can restore it on exit.
        let _ = ORIGINAL_CONSOLE_MODE.set(mode);
        // Explicitly set the mode we need rather than read-modify-write.
        // This eliminates the race with crossterm's EnableMouseCapture which
        // also calls GetConsoleMode/SetConsoleMode concurrently.
        //
        // Flags we set:
        //   ENABLE_WINDOW_INPUT           (0x0008) - receive window resize events
        //   ENABLE_MOUSE_INPUT            (0x0010) - receive mouse events; on ConPTY
        //                                            this signals the terminal emulator
        //                                            to capture and forward mouse input
        //   ENABLE_EXTENDED_FLAGS         (0x0080) - required to clear QUICK_EDIT
        //   ENABLE_VIRTUAL_TERMINAL_INPUT (0x0200) - stdin returns raw VT bytes
        //
        // Flags we deliberately clear:
        //   ENABLE_PROCESSED_INPUT  (0x0001) - let VT sequences through raw
        //   ENABLE_LINE_INPUT       (0x0002) - no line buffering
        //   ENABLE_ECHO_INPUT       (0x0004) - no echo
        //   ENABLE_QUICK_EDIT_MODE  (0x0040) - would intercept mouse events
        let new_mode = ENABLE_WINDOW_INPUT
            | ENABLE_MOUSE_INPUT
            | ENABLE_EXTENDED_FLAGS
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        if SetConsoleMode(handle, new_mode) == 0 {
            return false;
        }
        true
    }
}

/// Restore the console input mode that was saved by `enable_vt_input()`.
///
/// `crossterm::terminal::disable_raw_mode()` only adds back LINE_INPUT,
/// ECHO_INPUT and PROCESSED_INPUT — it never clears ENABLE_MOUSE_INPUT or
/// ENABLE_VIRTUAL_TERMINAL_INPUT.  If those flags are left set after Zellij
/// exits, ConPTY continues to deliver mouse events as VT escape sequences
/// into the shell's stdin, causing visible garbage like `[555;99;32M`.
pub(crate) fn restore_vt_input() {
    if let Some(&original_mode) = ORIGINAL_CONSOLE_MODE.get() {
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                SetConsoleMode(handle, original_mode);
            }
        }
    }
}

/// Windows native console event loop.
///
/// Uses crossterm's `event::read()` which reads INPUT_RECORDs via
/// ReadConsoleInput.  Works in cmd.exe, PowerShell, and Windows Terminal
/// where ALT is reported as a modifier flag.
///
/// Resize events are forwarded to the signal handler thread via `resize_sender`.
pub(crate) fn native_console_stdin_loop(
    send_input_instructions: SenderWithContext<InputInstruction>,
    resize_sender: Option<std::sync::mpsc::Sender<()>>,
    explicitly_disable_kitty_keyboard_protocol: bool,
) {
    let mut kitty_sequence_buffer =
        NativeKittySequenceBuffer::new(!explicitly_disable_kitty_keyboard_protocol);
    loop {
        if kitty_sequence_buffer.has_pending() {
            match event::poll(NATIVE_KITTY_SEQUENCE_TIMEOUT) {
                Ok(true) => {},
                Ok(false) => {
                    if send_key_dispatches(
                        &send_input_instructions,
                        kitty_sequence_buffer.flush_pending("timeout"),
                    ) {
                        break;
                    }
                    continue;
                },
                Err(e) => {
                    log::error!("Failed to poll crossterm event: {}", e);
                    let _ = send_input_instructions.send(InputInstruction::Exit);
                    break;
                },
            }
        }
        match event::read() {
            Ok(Event::Key(key_event)) => {
                if input_trace::enabled() {
                    log::info!("INPUT_TRACE native_console_key raw_event={:?}", key_event);
                }
                if key_event.kind != KeyEventKind::Press {
                    continue;
                }
                if let Some((key, bytes)) = cast_crossterm_key(key_event) {
                    if input_trace::enabled() {
                        log::info!(
                            "INPUT_TRACE native_console_cast key={:?} raw={}",
                            key,
                            input_trace::format_bytes(&bytes),
                        );
                    }
                    let dispatches = kitty_sequence_buffer.push(key, bytes);
                    if send_key_dispatches(&send_input_instructions, dispatches) {
                        break;
                    }
                } else {
                    if send_key_dispatches(
                        &send_input_instructions,
                        kitty_sequence_buffer.flush_pending("uncast_key_event"),
                    ) {
                        break;
                    }
                    if input_trace::enabled() {
                        log::info!("INPUT_TRACE native_console_cast skipped=true");
                    }
                }
            },
            Ok(Event::Mouse(mouse_event)) => {
                if send_key_dispatches(
                    &send_input_instructions,
                    kitty_sequence_buffer.flush_pending("non_key_event"),
                ) {
                    break;
                }
                let mouse_event = from_crossterm_mouse(mouse_event);
                if send_input_instructions
                    .send(InputInstruction::MouseEvent(mouse_event))
                    .is_err()
                {
                    break;
                }
            },
            Ok(Event::Paste(text)) => {
                if send_key_dispatches(
                    &send_input_instructions,
                    kitty_sequence_buffer.flush_pending("non_key_event"),
                ) {
                    break;
                }
                let raw_bytes = text.as_bytes().to_vec();
                let paste_event = InputEvent::Paste(text);
                if send_input_instructions
                    .send(InputInstruction::KeyEvent(paste_event, raw_bytes))
                    .is_err()
                {
                    break;
                }
            },
            Ok(Event::Resize(..)) => {
                if send_key_dispatches(
                    &send_input_instructions,
                    kitty_sequence_buffer.flush_pending("non_key_event"),
                ) {
                    break;
                }
                if let Some(ref tx) = resize_sender {
                    let _ = tx.send(());
                }
            },
            Ok(_) => {},
            Err(e) => {
                log::error!("Failed to read crossterm event: {}", e);
                let _ = send_input_instructions.send(InputInstruction::Exit);
                break;
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NativeKeyDispatch {
    key: KeyWithModifier,
    raw_bytes: Vec<u8>,
    is_kitty_keyboard_protocol: bool,
}

struct NativeKittySequenceBuffer {
    enabled: bool,
    pending: Vec<(KeyWithModifier, Vec<u8>)>,
}

impl NativeKittySequenceBuffer {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            pending: Vec::new(),
        }
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    fn push(&mut self, key: KeyWithModifier, raw_bytes: Vec<u8>) -> Vec<NativeKeyDispatch> {
        if !self.enabled {
            return vec![native_dispatch(key, raw_bytes)];
        }

        if self.pending.is_empty() {
            if is_native_kitty_sequence_start(&key, &raw_bytes) {
                self.pending.push((key, raw_bytes));
                return Vec::new();
            }
            return vec![native_dispatch(key, raw_bytes)];
        }

        if !is_native_kitty_sequence_continuation(&raw_bytes, self.pending.len()) {
            let mut dispatches = self.flush_pending("invalid_continuation");
            dispatches.push(native_dispatch(key, raw_bytes));
            return dispatches;
        }

        self.pending.push((key, raw_bytes));
        let sequence = self.pending_raw_bytes();
        if sequence.len() > NATIVE_KITTY_SEQUENCE_MAX_LEN {
            return self.flush_pending("too_long");
        }
        if !is_native_kitty_sequence_complete(&sequence) {
            return Vec::new();
        }

        let mut normalized = vec![0x1b];
        normalized.extend_from_slice(&sequence);
        match KittyKeyboardParser::new().feed(&normalized) {
            KittyParseOutcome::Complete(key) => {
                self.pending.clear();
                if input_trace::enabled() {
                    log::info!(
                        "INPUT_TRACE native_console_kitty_sequence outcome=Complete key={:?} raw={}",
                        key,
                        input_trace::format_bytes(&normalized),
                    );
                }
                vec![NativeKeyDispatch {
                    key,
                    raw_bytes: normalized,
                    is_kitty_keyboard_protocol: true,
                }]
            },
            KittyParseOutcome::Incomplete => Vec::new(),
            KittyParseOutcome::NoMatch => self.flush_pending("nomatch"),
        }
    }

    fn flush_pending(&mut self, reason: &'static str) -> Vec<NativeKeyDispatch> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        if input_trace::enabled() {
            log::info!(
                "INPUT_TRACE native_console_kitty_sequence outcome=Flush reason={} raw={}",
                reason,
                input_trace::format_bytes(&self.pending_raw_bytes()),
            );
        }
        self.pending
            .drain(..)
            .map(|(key, raw_bytes)| native_dispatch(key, raw_bytes))
            .collect()
    }

    fn pending_raw_bytes(&self) -> Vec<u8> {
        self.pending
            .iter()
            .flat_map(|(_, raw_bytes)| raw_bytes.iter().copied())
            .collect()
    }
}

fn native_dispatch(key: KeyWithModifier, raw_bytes: Vec<u8>) -> NativeKeyDispatch {
    NativeKeyDispatch {
        key,
        raw_bytes,
        is_kitty_keyboard_protocol: false,
    }
}

fn is_native_kitty_sequence_start(key: &KeyWithModifier, raw_bytes: &[u8]) -> bool {
    raw_bytes == b"[" && key.bare_key == BareKey::Char('[') && key.key_modifiers.is_empty()
}

fn is_native_kitty_sequence_continuation(raw_bytes: &[u8], pending_len: usize) -> bool {
    let [byte] = raw_bytes else {
        return false;
    };
    match *byte {
        b'0'..=b'9' => true,
        b';' => pending_len > 1,
        b'u' | b'~' => pending_len > 1,
        b'A'..=b'Z' => pending_len > 1,
        _ => false,
    }
}

fn is_native_kitty_sequence_complete(bytes: &[u8]) -> bool {
    matches!(bytes.last(), Some(b'u' | b'~' | b'A'..=b'Z'))
}

fn send_key_dispatches(
    send_input_instructions: &SenderWithContext<InputInstruction>,
    dispatches: Vec<NativeKeyDispatch>,
) -> bool {
    for dispatch in dispatches {
        if send_input_instructions
            .send(InputInstruction::KeyWithModifierEvent(
                dispatch.key,
                dispatch.raw_bytes,
                dispatch.is_kitty_keyboard_protocol,
            ))
            .is_err()
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod native_kitty_sequence_tests {
    use super::*;
    use zellij_utils::data::KeyModifier;

    fn key(c: char) -> KeyWithModifier {
        KeyWithModifier::new(BareKey::Char(c))
    }

    fn push_char(buffer: &mut NativeKittySequenceBuffer, c: char) -> Vec<NativeKeyDispatch> {
        buffer.push(key(c), c.to_string().into_bytes())
    }

    #[test]
    fn ctrl_dot_csi_u_sequence_emits_one_kitty_key() {
        let mut buffer = NativeKittySequenceBuffer::new(true);
        let mut dispatches = Vec::new();
        for c in "[46;5u".chars() {
            dispatches.extend(push_char(&mut buffer, c));
        }

        assert_eq!(dispatches.len(), 1);
        assert_eq!(
            dispatches[0].key,
            KeyWithModifier::new(BareKey::Char('.')).with_ctrl_modifier()
        );
        assert_eq!(dispatches[0].raw_bytes, b"\x1b[46;5u".to_vec());
        assert!(dispatches[0].is_kitty_keyboard_protocol);
    }

    #[test]
    fn ctrl_q_csi_u_sequence_emits_one_kitty_key() {
        let mut buffer = NativeKittySequenceBuffer::new(true);
        let mut dispatches = Vec::new();
        for c in "[113;5u".chars() {
            dispatches.extend(push_char(&mut buffer, c));
        }

        assert_eq!(dispatches.len(), 1);
        assert_eq!(
            dispatches[0].key,
            KeyWithModifier::new(BareKey::Char('q')).with_ctrl_modifier()
        );
        assert_eq!(dispatches[0].raw_bytes, b"\x1b[113;5u".to_vec());
        assert!(dispatches[0].is_kitty_keyboard_protocol);
    }

    #[test]
    fn regular_character_dispatches_immediately() {
        let mut buffer = NativeKittySequenceBuffer::new(true);

        let dispatches = push_char(&mut buffer, 'a');

        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].key, key('a'));
        assert_eq!(dispatches[0].raw_bytes, b"a".to_vec());
        assert!(!dispatches[0].is_kitty_keyboard_protocol);
        assert!(!buffer.has_pending());
    }

    #[test]
    fn literal_open_bracket_flushes_as_regular_key() {
        let mut buffer = NativeKittySequenceBuffer::new(true);

        assert!(push_char(&mut buffer, '[').is_empty());
        let dispatches = buffer.flush_pending("test");

        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].key, key('['));
        assert_eq!(dispatches[0].raw_bytes, b"[".to_vec());
        assert!(!dispatches[0].is_kitty_keyboard_protocol);
    }

    #[test]
    fn malformed_sequence_preserves_original_order() {
        let mut buffer = NativeKittySequenceBuffer::new(true);

        assert!(push_char(&mut buffer, '[').is_empty());
        let dispatches = push_char(&mut buffer, 'x');

        assert_eq!(dispatches.len(), 2);
        assert_eq!(dispatches[0].key, key('['));
        assert_eq!(dispatches[0].raw_bytes, b"[".to_vec());
        assert_eq!(dispatches[1].key, key('x'));
        assert_eq!(dispatches[1].raw_bytes, b"x".to_vec());
        assert!(dispatches.iter().all(|d| !d.is_kitty_keyboard_protocol));
    }

    #[test]
    fn disabled_buffer_dispatches_sequence_characters_normally() {
        let mut buffer = NativeKittySequenceBuffer::new(false);

        let dispatches = push_char(&mut buffer, '[');

        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].key, key('['));
        assert_eq!(dispatches[0].raw_bytes, b"[".to_vec());
        assert!(!dispatches[0].is_kitty_keyboard_protocol);
        assert!(!buffer.has_pending());
    }

    #[test]
    fn restored_ctrl_modifier_does_not_depend_on_crossterm_modifier_flags() {
        let mut buffer = NativeKittySequenceBuffer::new(true);
        let mut dispatches = Vec::new();
        for c in "[46;5u".chars() {
            dispatches.extend(push_char(&mut buffer, c));
        }

        assert!(dispatches[0].key.key_modifiers.contains(&KeyModifier::Ctrl));
    }
}
