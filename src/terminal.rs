use vt100::{Parser, Screen};

use crate::protocol::CopyMode;

pub struct TerminalState {
    parser: Parser,
}

impl TerminalState {
    pub fn with_limits(
        rows: u16,
        cols: u16,
        scrollback_lines: usize,
        scrollback_bytes: usize,
    ) -> Self {
        // Keep the parser's cell history under the configured byte budget by
        // reserving a conservative amount per column. The limit is conservative
        // because terminal attributes and wide graphemes cost more than text.
        let estimated_bytes_per_line = usize::from(cols).max(1).saturating_mul(8);
        let byte_limited_lines = (scrollback_bytes / estimated_bytes_per_line).max(1);
        let history_lines = scrollback_lines.min(byte_limited_lines);
        Self {
            parser: Parser::new(rows, cols, history_lines),
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    pub fn set_scrollback(&mut self, rows: usize) {
        self.parser.screen_mut().set_scrollback(rows);
    }

    pub fn scroll(&mut self, delta: i32) {
        let current = self.parser.screen().scrollback();
        let target = if delta.is_positive() {
            current.saturating_add(delta as usize)
        } else {
            current.saturating_sub(delta.unsigned_abs() as usize)
        };
        self.set_scrollback(target);
    }

    pub fn scroll_to_top(&mut self) {
        self.set_scrollback(usize::MAX);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.set_scrollback(0);
    }

    pub fn is_scrolled(&self) -> bool {
        self.parser.screen().scrollback() > 0
    }

    pub fn has_scrollback(&mut self) -> bool {
        let original = self.parser.screen().scrollback();
        self.set_scrollback(usize::MAX);
        let available = self.parser.screen().scrollback() > 0;
        self.set_scrollback(original);
        available
    }

    #[cfg(test)]
    pub fn search(&mut self, query: &str, direction: i8) -> bool {
        if query.is_empty() {
            return false;
        }
        let original = self.parser.screen().scrollback();
        let maximum = self.maximum_scrollback();
        if direction >= 0 {
            for offset in (original.saturating_add(1)..=maximum).chain(0..=original) {
                self.set_scrollback(offset);
                if self.parser.screen().contents().contains(query) {
                    return true;
                }
            }
        } else {
            for offset in (0..original)
                .rev()
                .chain((original.saturating_add(1)..=maximum).rev())
            {
                self.set_scrollback(offset);
                if self.parser.screen().contents().contains(query) {
                    return true;
                }
            }
        }
        self.set_scrollback(original);
        false
    }

    #[cfg(test)]
    fn maximum_scrollback(&mut self) -> usize {
        let original = self.parser.screen().scrollback();
        self.set_scrollback(usize::MAX);
        let maximum = self.parser.screen().scrollback();
        self.set_scrollback(original);
        maximum
    }

    pub fn selection_text(
        &self,
        start_row: u16,
        start_col: u16,
        end_row: u16,
        end_col: u16,
        mode: CopyMode,
    ) -> String {
        let (start_row, start_col, end_row, end_col) =
            if (start_row, start_col) <= (end_row, end_col) {
                (start_row, start_col, end_row, end_col)
            } else {
                (end_row, end_col, start_row, start_col)
            };
        match mode {
            CopyMode::Character => self
                .parser
                .screen()
                .contents_between(start_row, start_col, end_row, end_col),
            CopyMode::Line => self.parser.screen().contents_between(
                start_row,
                0,
                end_row,
                self.parser.screen().size().1,
            ),
            CopyMode::Rectangle => self
                .parser
                .screen()
                .rows(start_col, end_col.saturating_sub(start_col))
                .skip(usize::from(start_row))
                .take(usize::from(end_row - start_row + 1))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    pub fn screen(&self) -> &Screen {
        self.parser.screen()
    }

    #[cfg(test)]
    pub fn contents(&self) -> String {
        self.parser.screen().contents()
    }
}

#[cfg(test)]
mod tests {
    use super::TerminalState;
    use crate::protocol::CopyMode;
    use vt100::Color;

    #[test]
    fn parses_text_and_attributes() {
        let mut terminal = TerminalState::with_limits(4, 20, 10, usize::MAX);
        terminal.process(b"hello \x1b[31mred\x1b[0m");

        assert_eq!(terminal.screen().cell(0, 0).unwrap().contents(), "h");
        assert_eq!(terminal.screen().cell(0, 6).unwrap().contents(), "r");
        assert_eq!(
            terminal.screen().cell(0, 6).unwrap().fgcolor(),
            Color::Idx(1)
        );
        assert!(terminal.contents().contains("hello red"));
    }

    #[test]
    fn resizes_terminal() {
        let mut terminal = TerminalState::with_limits(4, 20, 10, usize::MAX);
        terminal.resize(8, 40);
        assert_eq!(terminal.screen().size(), (8, 40));
    }

    #[test]
    fn retains_scrollback() {
        let mut terminal = TerminalState::with_limits(2, 20, 10, usize::MAX);
        terminal.process(b"one\ntwo\nthree\nfour\n");
        terminal.set_scrollback(1);
        assert_eq!(terminal.screen().scrollback(), 1);
        assert!(terminal.has_scrollback());
    }

    #[test]
    fn scrolls_through_multiple_pages() {
        let mut terminal = TerminalState::with_limits(24, 80, 200, usize::MAX);
        for index in 0..200 {
            let line = format!("line-{index}\n");
            terminal.process(line.as_bytes());
        }

        terminal.scroll(12);
        terminal.scroll(12);
        assert_eq!(terminal.screen().scrollback(), 24);
        assert!(terminal.contents().contains("line-175"));

        terminal.scroll_to_top();
        assert!(terminal.contents().contains("line-0"));
    }

    #[test]
    fn preserves_scrollback_for_top_anchored_partial_region() {
        let mut terminal = TerminalState::with_limits(5, 40, 20, usize::MAX);
        terminal.process(b"\x1b[1;3rfirst-1\r\nfirst-2\r\nfirst-3\r\nsecond-1\r\nsecond-2");

        terminal.scroll_to_top();
        assert!(terminal.contents().contains("first-1"));
    }

    #[test]
    fn excludes_non_top_partial_region_from_scrollback() {
        let mut terminal = TerminalState::with_limits(5, 40, 20, usize::MAX);
        terminal.process(b"\x1b[2;4r\x1b[2;1Hfirst\r\nsecond\r\nthird\r\nfourth");

        assert!(!terminal.has_scrollback());
        assert!(!terminal.contents().contains("first"));
    }

    #[test]
    fn preserves_lines_deleted_from_top_anchored_region() {
        let mut terminal = TerminalState::with_limits(5, 40, 20, usize::MAX);
        terminal.process(b"\x1b[1;3rfirst\r\nsecond\r\nthird\x1b[1;1H\x1b[M");

        terminal.scroll_to_top();
        assert!(terminal.contents().contains("first"));
    }

    #[test]
    fn scrolls_and_searches_history() {
        let mut terminal = TerminalState::with_limits(2, 20, 10, usize::MAX);
        terminal.process(b"alpha\nbeta\ngamma\n");
        terminal.scroll_to_top();
        assert!(terminal.search("alpha", 1));
        terminal.scroll_to_bottom();
        assert_eq!(terminal.screen().scrollback(), 0);
    }

    #[test]
    fn handles_large_and_malformed_output() {
        let mut terminal = TerminalState::with_limits(24, 80, 20_000, usize::MAX);
        for index in 0..10_000 {
            let line = format!("line-{index}\n");
            terminal.process(line.as_bytes());
        }
        terminal.process(b"\x1b[31;\xffmtruncated\x1b[0m\n");
        terminal.scroll_to_top();
        assert!(terminal.search("line-0", 1));
        terminal.resize(40, 120);
        assert_eq!(terminal.screen().size(), (40, 120));
    }

    #[test]
    fn copies_character_line_and_rectangle_ranges() {
        let mut terminal = TerminalState::with_limits(4, 20, 10, usize::MAX);
        terminal.process(b"abcdef\r\nghijkl\r\nmnopqr\r\n");
        assert_eq!(
            terminal.selection_text(0, 1, 0, 4, CopyMode::Character),
            "bcd"
        );
        assert!(terminal
            .selection_text(0, 0, 1, 20, CopyMode::Line)
            .contains("abcdef"));
        assert_eq!(
            terminal.selection_text(0, 1, 1, 4, CopyMode::Rectangle),
            "bcd\nhij"
        );
    }

    #[test]
    fn restores_primary_screen_after_alternate_screen() {
        let mut terminal = TerminalState::with_limits(4, 20, 10, usize::MAX);
        terminal.process(b"primary\x1b[?1049halt\x1b[2J\x1b[?1049l");
        assert!(terminal.contents().contains("primary"));
        assert!(!terminal.contents().contains("alt"));
    }
}
