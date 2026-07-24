use plux::{protocol::CopyMode, terminal::TerminalState};
use vt100::Color;

#[test]
fn ansi_golden_snapshot_is_stable() {
    let mut terminal = TerminalState::with_limits(3, 12, 10, usize::MAX);
    terminal.process(b"one\r\ntwo\x1b[31m!\x1b[0m\x1b[3;1Hdone");

    assert_eq!(terminal.screen().cell(0, 0).unwrap().contents(), "o");
    assert_eq!(terminal.screen().cell(1, 3).unwrap().contents(), "!");
    assert_eq!(
        terminal.screen().cell(1, 3).unwrap().fgcolor(),
        Color::Idx(1)
    );
    assert_eq!(terminal.screen().cell(2, 0).unwrap().contents(), "d");
    assert_eq!(
        terminal.selection_text(0, 0, 1, 3, CopyMode::Character),
        "one\ntwo"
    );
}
