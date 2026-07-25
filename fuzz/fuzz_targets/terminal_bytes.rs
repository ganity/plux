#![no_main]

use libfuzzer_sys::fuzz_target;
use plux::terminal::TerminalState;

fuzz_target!(|bytes: &[u8]| {
    let mut terminal = TerminalState::with_limits(24, 80, 20_000, 64 * 1024 * 1024);
    for (index, chunk) in bytes.chunks(32).enumerate() {
        terminal.process(chunk);
        if index % 2 == 0 && !chunk.is_empty() {
            let rows = 2 + u16::from(chunk[0] % 79);
            let cols = 1 + u16::from(chunk[chunk.len() - 1] % 240);
            terminal.resize(rows, cols);
            if index % 4 == 0 {
                terminal.scroll_to_top();
            } else {
                terminal.scroll_to_bottom();
            }
        }
    }
});
