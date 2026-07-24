#![no_main]

use libfuzzer_sys::fuzz_target;
use plux::terminal::TerminalState;

fuzz_target!(|bytes: &[u8]| {
    let mut terminal = TerminalState::with_limits(24, 80, 20_000, 64 * 1024 * 1024);
    terminal.process(bytes);
});

