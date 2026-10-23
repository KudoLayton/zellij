use super::*;
use crate::terminal_output::{TerminalOutput, TerminalStreamGuard};

fn packet(guard: &TerminalStreamGuard, seq: u64, text: &[u8]) -> TerminalOutput {
    TerminalOutput::new(1, text.to_vec(), seq, guard.clone())
}

fn viewport(screen: &Screen) -> Vec<String> {
    screen
        .tabs
        .values()
        .find_map(|tab| tab.get_pane_with_id(PaneId::Terminal(1)))
        .unwrap()
        .pane_contents(None, false, None)
        .viewport
}

#[test]
fn invalidated_output_is_rejected_before_replacement_produces_any_bytes() {
    let (mut screen, _) = create_new_screen_with_message_capture(Size { cols: 80, rows: 20 });
    new_tab(&mut screen, 1, 0);
    let guard = TerminalStreamGuard::new();
    let old = packet(&guard, 0, b"must-not-render");
    let before = viewport(&screen);
    guard.invalidate();
    assert!(screen.apply_terminal_output(old).unwrap().is_none());
    assert_eq!(viewport(&screen), before);
    screen
        .apply_terminal_output(packet(&TerminalStreamGuard::new(), 0, b"fresh-output"))
        .unwrap();
    assert!(viewport(&screen).join("\n").contains("fresh-output"));
}

#[test]
fn duplicate_output_is_not_parsed_twice() {
    let (mut screen, _) = create_new_screen_with_message_capture(Size { cols: 80, rows: 20 });
    new_tab(&mut screen, 1, 0);
    let guard = TerminalStreamGuard::new();
    let output = packet(&guard, 0, b"unique");
    screen.apply_terminal_output(output.clone()).unwrap();
    screen.apply_terminal_output(output).unwrap();
    assert_eq!(viewport(&screen).join("\n").matches("unique").count(), 1);
}

#[test]
fn delayed_pane_creation_keeps_the_generation_guard() {
    let (mut screen, _) = create_new_screen_with_message_capture(Size { cols: 80, rows: 20 });
    let guard = TerminalStreamGuard::new();
    let output = screen
        .apply_terminal_output(packet(&guard, 0, b"stale-pending"))
        .unwrap()
        .unwrap();
    let mut pending = HashMap::from([(
        PaneId::Terminal(1),
        vec![ScreenInstruction::PtyOutput(output)],
    )]);
    guard.invalidate();
    new_tab(&mut screen, 1, 0);
    screen.finish_pane_creation(PaneId::Terminal(1), &mut None, &mut pending);
    assert!(pending.is_empty());
    assert!(!viewport(&screen).join("\n").contains("stale-pending"));
}

#[test]
fn gap_forces_a_snapshot_even_when_viewport_is_unchanged() {
    let (mut screen, messages) =
        create_new_screen_with_message_capture(Size { cols: 80, rows: 20 });
    new_tab(&mut screen, 1, 0);
    screen.subscribe_to_pane_renders(
        100,
        vec![zellij_utils::data::PaneId::Terminal(1)],
        None,
        false,
    );
    let initial = viewport(&screen);
    let guard = TerminalStreamGuard::new();
    // SGR reset changes no visible cells; missing sequence 1 still needs recovery.
    screen
        .apply_terminal_output(packet(&guard, 0, b"\x1b[0m"))
        .unwrap();
    screen
        .apply_terminal_output(packet(&guard, 2, b"\x1b[0m"))
        .unwrap();
    let map = HashMap::from([(
        zellij_utils::data::PaneId::Terminal(1),
        PaneContents {
            viewport: initial,
            ..Default::default()
        },
    )]);
    screen.deliver_subscriber_updates_from_map(&map, None);
    screen.deliver_subscriber_updates_from_map(&map, None);
    let messages = messages.lock().unwrap();
    assert_eq!(
        messages[&100]
            .iter()
            .filter(|m| matches!(m, ServerToClientMsg::PaneRenderUpdate { .. }))
            .count(),
        2
    );
}

#[test]
fn generation_reset_recovers_an_ahead_subscriber() {
    let (mut screen, messages) =
        create_new_screen_with_message_capture(Size { cols: 80, rows: 20 });
    new_tab(&mut screen, 1, 0);
    let old = TerminalStreamGuard::new();
    screen
        .apply_terminal_output(packet(&old, 100, b"\x1b[0m"))
        .unwrap();
    screen.subscribe_to_pane_renders(
        100,
        vec![zellij_utils::data::PaneId::Terminal(1)],
        None,
        false,
    );
    old.invalidate();
    screen
        .apply_terminal_output(packet(&TerminalStreamGuard::new(), 0, b"\x1b[0m"))
        .unwrap();
    screen.deliver_to_pane_subscribers_directly();
    assert_eq!(messages.lock().unwrap()[&100].len(), 2);
}

#[test]
fn failed_initial_snapshot_does_not_register_a_subscriber() {
    let (mut screen, _) = create_new_screen_with_message_capture(Size { cols: 80, rows: 20 });
    new_tab(&mut screen, 1, 0);
    let io = FakeInputOutput::default();
    io.fail_client_sends
        .store(true, std::sync::atomic::Ordering::SeqCst);
    screen.bus.os_input = Some(Box::new(io));
    screen.subscribe_to_pane_renders(
        100,
        vec![zellij_utils::data::PaneId::Terminal(1)],
        None,
        false,
    );
    assert!(!screen.pane_render_subscribers.contains_key(&100));
}

#[test]
fn failed_update_or_close_removes_even_multi_pane_subscribers() {
    for close in [false, true] {
        let (mut screen, _) = create_new_screen_with_message_capture(Size { cols: 80, rows: 20 });
        new_tab(&mut screen, 1, 0);
        new_tab(&mut screen, 2, 1);
        let io = FakeInputOutput::default();
        let fail = io.fail_client_sends.clone();
        screen.bus.os_input = Some(Box::new(io));
        screen.subscribe_to_pane_renders(
            100,
            vec![
                zellij_utils::data::PaneId::Terminal(1),
                zellij_utils::data::PaneId::Terminal(2),
            ],
            None,
            false,
        );
        fail.store(true, std::sync::atomic::Ordering::SeqCst);
        if close {
            screen.notify_pane_closed_to_subscribers(zellij_utils::data::PaneId::Terminal(1));
        } else {
            screen
                .apply_terminal_output(packet(&TerminalStreamGuard::new(), 0, b"changed"))
                .unwrap();
            screen.deliver_to_pane_subscribers_directly();
        }
        assert!(!screen.pane_render_subscribers.contains_key(&100));
    }
}

#[test]
fn client_removal_is_idempotent_with_closed_plugin_channel() {
    let (mut screen, _) = create_new_screen_with_message_capture(Size { cols: 80, rows: 20 });
    new_tab(&mut screen, 1, 0);
    let (tx, rx) = crossbeam::channel::unbounded();
    drop(rx);
    screen.bus.senders.to_plugin = Some(SenderWithContext::new(tx));
    screen.bus.senders.should_silently_fail = false;
    screen.remove_client(1).unwrap();
    screen.remove_client(1).unwrap();
    assert!(!screen.connected_clients.borrow().contains_key(&1));
    assert!(!screen.client_sizes.contains_key(&1));
}

#[test]
fn invalid_resubscription_removes_previous_subscription() {
    let (mut screen, _) = create_new_screen_with_message_capture(Size { cols: 80, rows: 20 });
    new_tab(&mut screen, 1, 0);
    screen.subscribe_to_pane_renders(
        100,
        vec![zellij_utils::data::PaneId::Terminal(1)],
        None,
        false,
    );
    screen.subscribe_to_pane_renders(
        100,
        vec![zellij_utils::data::PaneId::Terminal(999)],
        None,
        false,
    );
    assert!(!screen.pane_render_subscribers.contains_key(&100));
}

#[test]
fn pane_close_removes_cursor_and_viewport_without_dropping_other_subscription() {
    let (mut screen, _) = create_new_screen_with_message_capture(Size { cols: 80, rows: 20 });
    new_tab(&mut screen, 1, 0);
    new_tab(&mut screen, 2, 1);
    screen.subscribe_to_pane_renders(
        100,
        vec![
            zellij_utils::data::PaneId::Terminal(1),
            zellij_utils::data::PaneId::Terminal(2),
        ],
        None,
        false,
    );
    screen.notify_pane_closed_to_subscribers(zellij_utils::data::PaneId::Terminal(1));
    let sub = &screen.pane_render_subscribers[&100];
    assert!(!sub.output_cursors.contains_key(&1));
    assert!(!sub
        .previous_viewports
        .contains_key(&zellij_utils::data::PaneId::Terminal(1)));
    assert!(sub
        .pane_ids
        .contains(&zellij_utils::data::PaneId::Terminal(2)));
}
