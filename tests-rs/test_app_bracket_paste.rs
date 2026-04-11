use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, KeyEventKind, KeyEventState};

fn mk(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn feed_str(state: &mut State, s: &str) -> Vec<Action> {
    s.chars().map(|c| {
        let key = if c == '\x1b' { mk(KeyCode::Esc) }
                  else if c == '\r' { mk(KeyCode::Enter) }
                  else { mk(KeyCode::Char(c)) };
        feed(state, key)
    }).collect()
}

#[test]
fn simple_paste() {
    let mut st = State::new();
    let actions = feed_str(&mut st, "\x1b[200~hello\x1b[201~");
    // All but the last should be Consumed; last should be Paste("hello")
    let last = actions.last().unwrap();
    match last {
        Action::Paste(text) => assert_eq!(text, "hello"),
        _ => panic!("expected Paste, got something else"),
    }
    for a in &actions[..actions.len()-1] {
        assert!(matches!(a, Action::Consumed));
    }
}

#[test]
fn multiline_paste_preserves_indentation() {
    let mut st = State::new();
    let payload = "line1\r   indented\r      more\r";
    let full = format!("\x1b[200~{}\x1b[201~", payload);
    let actions = feed_str(&mut st, &full);
    match actions.last().unwrap() {
        Action::Paste(text) => {
            assert_eq!(text, payload);
            // Verify indentation preserved exactly
            let lines: Vec<&str> = text.split('\r').collect();
            assert!(lines[1].starts_with("   indented"));
            assert!(lines[2].starts_with("      more"));
        }
        _ => panic!("expected Paste"),
    }
}

#[test]
fn aborted_open_replays_keys() {
    let mut st = State::new();
    // Send partial open sequence then a non-matching char
    let actions = feed_str(&mut st, "\x1b[2x");
    // First 3 (\x1b, [, 2) are consumed, then 'x' triggers Replay
    assert!(matches!(actions[0], Action::Consumed));
    assert!(matches!(actions[1], Action::Consumed));
    assert!(matches!(actions[2], Action::Consumed));
    match &actions[3] {
        Action::Replay(pending, current) => {
            assert_eq!(pending.len(), 3); // ESC, [, 2
            assert_eq!(current.code, KeyCode::Char('x'));
        }
        _ => panic!("expected Replay"),
    }
}

#[test]
fn non_esc_forwarded() {
    let mut st = State::new();
    let actions = feed_str(&mut st, "abc");
    for a in &actions {
        assert!(matches!(a, Action::Forward(_)));
    }
}

#[test]
fn esc_in_paste_is_not_close() {
    // ESC inside paste followed by non-[ should be captured
    let mut st = State::new();
    let full = "\x1b[200~before\x1bxafter\x1b[201~";
    let actions = feed_str(&mut st, full);
    match actions.last().unwrap() {
        Action::Paste(text) => {
            assert!(text.contains("\x1bx"));
            assert!(text.contains("before"));
            assert!(text.contains("after"));
        }
        _ => panic!("expected Paste"),
    }
}

#[test]
fn large_paste_content() {
    let mut st = State::new();
    // Build a large payload with varied indentation
    let mut payload = String::new();
    for i in 0..200 {
        let indent = " ".repeat(i % 8);
        payload.push_str(&format!("{}line {}\r", indent, i));
    }
    let full = format!("\x1b[200~{}\x1b[201~", payload);
    let actions = feed_str(&mut st, &full);
    match actions.last().unwrap() {
        Action::Paste(text) => {
            assert_eq!(text, &payload);
            assert_eq!(text.matches('\r').count(), 200);
        }
        _ => panic!("expected Paste"),
    }
}

#[test]
fn consecutive_pastes() {
    let mut st = State::new();
    // First paste
    let a1 = feed_str(&mut st, "\x1b[200~first\x1b[201~");
    match a1.last().unwrap() {
        Action::Paste(t) => assert_eq!(t, "first"),
        _ => panic!("expected Paste"),
    }
    // Normal key between pastes
    let a2 = feed_str(&mut st, "x");
    assert!(matches!(a2[0], Action::Forward(_)));
    // Second paste
    let a3 = feed_str(&mut st, "\x1b[200~second\x1b[201~");
    match a3.last().unwrap() {
        Action::Paste(t) => assert_eq!(t, "second"),
        _ => panic!("expected Paste"),
    }
}

#[test]
fn timeout_flush_enters_post_flush_drain() {
    // Simulate a paste where the close sequence is lost:
    // open sequence arrives, text accumulated, then timeout fires.
    // After timeout, the `~` from a stripped close sequence should
    // NOT leak through as a visible character (issue #197).
    let mut st = State::new();
    // Feed the open sequence + paste content (no close sequence).
    let _ = feed_str(&mut st, "\x1b[200~hello world");
    assert!(matches!(st, State::Pasting { .. }));
    // Simulate: wait for the paste timeout to expire.  We cannot
    // easily wait 2 seconds in a unit test, so we manipulate the
    // started timestamp.
    if let State::Pasting { ref mut started, .. } = st {
        *started = std::time::Instant::now() - std::time::Duration::from_secs(3);
    }
    let timeout_result = flush_timeout(&mut st);
    match timeout_result {
        TimeoutAction::FlushPaste(text) => {
            assert_eq!(text, "hello world");
        }
        _ => panic!("expected FlushPaste after timeout"),
    }
    // State should be IdlePostFlush (drain mode), not plain Idle.
    assert!(matches!(st, State::IdlePostFlush { .. }));
    // A `~` arriving now should be absorbed (it is residue from the
    // stripped close sequence \x1b[201~).
    let action = feed(&mut st, mk(KeyCode::Char('~')));
    assert!(matches!(action, Action::Consumed),
        "tilde after paste flush should be absorbed, got {:?}", std::mem::discriminant(&action));
    // A normal character should pass through and transition to Idle.
    let action = feed(&mut st, mk(KeyCode::Char('a')));
    assert!(matches!(action, Action::Forward(_)),
        "normal char after drain should be forwarded");
    assert!(matches!(st, State::Idle));
}

#[test]
fn timeout_flush_from_match_close_absorbs_residue() {
    // Close sequence partially matched, then timeout fires.
    let mut st = State::new();
    // Open + content + partial close: \x1b[20 (missing 1~)
    let _ = feed_str(&mut st, "\x1b[200~data\x1b[20");
    assert!(matches!(st, State::MatchClose { .. }));
    // Expire the timeout
    if let State::MatchClose { ref mut started, .. } = st {
        *started = std::time::Instant::now() - std::time::Duration::from_secs(3);
    }
    let result = flush_timeout(&mut st);
    match result {
        TimeoutAction::FlushPaste(text) => assert_eq!(text, "data"),
        _ => panic!("expected FlushPaste"),
    }
    assert!(matches!(st, State::IdlePostFlush { .. }));
    // Remaining close chars should be absorbed
    let a1 = feed(&mut st, mk(KeyCode::Char('1')));
    assert!(matches!(a1, Action::Consumed));
    let a2 = feed(&mut st, mk(KeyCode::Char('~')));
    assert!(matches!(a2, Action::Consumed));
}
