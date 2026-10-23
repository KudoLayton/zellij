use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[test]
fn literal_csi_u_text_remains_literal_not_a_shortcut() {
    let mut bytes = Vec::new();
    for character in "[113;5u".chars() {
        match native_key_instruction(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)) {
            Some(InputInstruction::KeyWithModifierEvent(_, raw, false)) => bytes.extend(raw),
            other => panic!("expected literal native key, got {other:?}"),
        }
    }
    assert_eq!(bytes, b"[113;5u");
}

#[test]
fn native_ctrl_modifier_is_not_lost() {
    let event = KeyEvent::new(KeyCode::Char('.'), KeyModifiers::CONTROL);
    match native_key_instruction(event) {
        Some(InputInstruction::KeyWithModifierEvent(key, _, false)) => {
            assert!(key
                .key_modifiers
                .contains(&zellij_utils::data::KeyModifier::Ctrl));
        },
        other => panic!("expected native Ctrl key, got {other:?}"),
    }
}

#[test]
fn native_key_release_does_not_dispatch_again() {
    let mut event = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
    event.kind = KeyEventKind::Release;
    assert!(native_key_instruction(event).is_none());
}
