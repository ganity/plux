use std::{
    collections::HashMap,
    io::Write,
    sync::mpsc::Sender,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

use crate::{
    config::Config,
    error::Result,
    layout::{Border, FocusDirection, LayoutNode, PaneRect, SplitDirection},
    pane::{Pane, PaneEvent},
};

pub struct Session {
    pub name: String,
    pub panes: HashMap<u64, Pane>,
    pub layout: LayoutNode,
    pub focused: u64,
    pub attached: bool,
    pub exited: HashMap<u64, String>,
    rows: u16,
    cols: u16,
    zoomed: bool,
    mouse_capture: bool,
    pub temporary: bool,
    pub ever_attached: bool,
    created_at: u64,
    last_attached_at: u64,
    rendered_rows: HashMap<u64, Vec<Vec<u8>>>,
    force_full_render: bool,
}

#[derive(Debug, Serialize)]
pub struct SessionMetadata {
    pub name: String,
    pub pane_ids: Vec<u64>,
    pub panes: Vec<PaneMetadata>,
    pub focused: u64,
    pub layout: LayoutNode,
    pub created_at: u64,
    pub last_attached_at: u64,
    pub temporary: bool,
}

pub struct SessionOptions {
    pub command: Option<Vec<String>>,
    pub temporary: bool,
}

#[derive(Debug, Serialize)]
pub struct PaneMetadata {
    pub id: u64,
    pub command: Vec<String>,
    pub process_id: Option<u32>,
    pub cwd: String,
    pub exited: Option<String>,
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(
            "session name must be 1-64 characters: letters, digits, '.', '-' or '_'".into(),
        );
    }
    Ok(())
}

impl Session {
    pub fn new_with_command(
        name: String,
        pane_id: u64,
        config: &Config,
        rows: u16,
        cols: u16,
        options: SessionOptions,
        events: Sender<PaneEvent>,
    ) -> Result<Self> {
        let pane =
            Pane::spawn_with_session(pane_id, config, rows, cols, &name, options.command, events)?;
        let mut panes = HashMap::new();
        panes.insert(pane_id, pane);
        Ok(Self {
            name,
            panes,
            layout: LayoutNode::leaf(pane_id),
            focused: pane_id,
            attached: false,
            exited: HashMap::new(),
            rows,
            cols,
            zoomed: false,
            mouse_capture: config.mouse,
            temporary: options.temporary,
            ever_attached: false,
            created_at: unix_seconds(),
            last_attached_at: 0,
            rendered_rows: HashMap::new(),
            force_full_render: true,
        })
    }

    pub fn focused_pane_id(&self) -> u64 {
        self.focused
    }

    pub fn metadata(&self) -> SessionMetadata {
        let mut pane_ids = self.panes.keys().copied().collect::<Vec<_>>();
        pane_ids.sort_unstable();
        let mut panes = self
            .panes
            .iter()
            .map(|(id, pane)| PaneMetadata {
                id: *id,
                command: pane.command.clone(),
                process_id: pane.process_id,
                cwd: pane.cwd.display().to_string(),
                exited: self.exited.get(id).cloned(),
            })
            .collect::<Vec<_>>();
        panes.sort_by_key(|pane| pane.id);
        SessionMetadata {
            name: self.name.clone(),
            pane_ids,
            panes,
            focused: self.focused,
            layout: self.layout.clone(),
            created_at: self.created_at,
            last_attached_at: self.last_attached_at,
            temporary: self.temporary,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.ever_attached
            && !self.panes.is_empty()
            && self.panes.keys().all(|id| self.exited.contains_key(id))
    }

    pub fn mark_attached(&mut self) {
        self.last_attached_at = unix_seconds();
        self.attached = true;
        self.ever_attached = true;
        self.force_full_render = true;
    }

    pub fn rename(&mut self, name: String) {
        self.name = name;
    }

    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    pub fn focused_mouse_enabled(&self) -> bool {
        self.panes.get(&self.focused).is_some_and(|pane| {
            pane.terminal.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None
        })
    }

    pub fn focused_alternate_screen(&self) -> bool {
        self.panes
            .get(&self.focused)
            .is_some_and(|pane| pane.terminal.screen().alternate_screen())
    }

    pub fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        self.panes.get_mut(&self.focused)
    }

    pub fn pane_mut(&mut self, pane_id: u64) -> Option<&mut Pane> {
        self.panes.get_mut(&pane_id)
    }

    pub fn has_pane(&self, pane_id: u64) -> bool {
        self.panes.contains_key(&pane_id)
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.rows = rows.max(2);
        self.cols = cols.max(1);
        self.force_full_render = true;
        for rect in self.rectangles() {
            if let Some(pane) = self.panes.get_mut(&rect.pane_id) {
                pane.resize(rect.rows, rect.cols)?;
            }
        }
        Ok(())
    }

    pub fn split(
        &mut self,
        new_pane_id: u64,
        direction: SplitDirection,
        config: &Config,
        events: Sender<PaneEvent>,
    ) -> Result<()> {
        let old_focused = self.focused;
        let focused_rect = self
            .rectangles()
            .into_iter()
            .find(|rect| rect.pane_id == old_focused)
            .ok_or("focused pane is missing from layout")?;
        let enough_space = match direction {
            SplitDirection::Vertical => focused_rect.cols >= 21,
            SplitDirection::Horizontal => focused_rect.rows >= 7,
        };
        if !enough_space {
            return Err("pane is too small to split (minimum 10x3 per pane)".into());
        }
        let pane = Pane::spawn_with_session(
            new_pane_id,
            config,
            self.rows,
            self.cols,
            &self.name,
            None,
            events,
        )?;
        if !self.layout.split_leaf(old_focused, new_pane_id, direction) {
            return Err("focused pane is missing from layout".into());
        }
        self.panes.insert(new_pane_id, pane);
        self.focused = new_pane_id;
        self.zoomed = false;
        self.force_full_render = true;
        self.resize(self.rows, self.cols)
    }

    pub fn adjust_ratio(&mut self, delta: i16) -> Result<()> {
        if !self.layout.adjust_ratio_for(self.focused, delta) {
            return Err("focused pane is missing from layout".into());
        }
        self.resize(self.rows, self.cols)
    }

    pub fn close_focused(&mut self) -> Result<bool> {
        if self.panes.len() == 1 {
            return Ok(false);
        }
        let pane_id = self.focused;
        if !self.layout.close_leaf(pane_id) {
            return Err("focused pane is missing from layout".into());
        }
        if let Some(mut pane) = self.panes.remove(&pane_id) {
            let _ = pane.kill();
        }
        let mut pane_ids = Vec::new();
        self.layout.pane_ids(&mut pane_ids);
        self.focused = pane_ids.first().copied().ok_or("layout has no panes")?;
        self.zoomed = false;
        self.force_full_render = true;
        self.resize(self.rows, self.cols)?;
        Ok(true)
    }

    pub fn focus(&mut self, direction: FocusDirection) {
        if let Some(pane_id) =
            self.layout
                .focus_neighbor(self.focused, direction, self.rows, self.cols)
        {
            self.focused = pane_id;
            self.force_full_render = true;
        }
    }

    pub fn toggle_zoom(&mut self) {
        self.zoomed = !self.zoomed;
        self.force_full_render = true;
    }

    pub fn record_exit(&mut self, pane_id: u64, status: String) {
        self.exited.insert(pane_id, status);
    }

    pub fn restart_focused_if_exited(
        &mut self,
        config: &Config,
        events: Sender<PaneEvent>,
    ) -> Result<bool> {
        let pane_id = self.focused;
        let Some(old_pane) = self.panes.get(&pane_id) else {
            return Ok(false);
        };
        if !self.exited.contains_key(&pane_id) {
            return Ok(false);
        }
        let command = old_pane.command.clone();
        let new_pane = Pane::spawn_with_session(
            pane_id,
            config,
            self.rows,
            self.cols,
            &self.name,
            Some(command),
            events,
        )?;
        let _old_pane = self.panes.insert(pane_id, new_pane);
        self.exited.remove(&pane_id);
        self.resize(self.rows, self.cols)?;
        Ok(true)
    }

    pub fn rectangles(&self) -> Vec<PaneRect> {
        if self.zoomed {
            return vec![PaneRect {
                pane_id: self.focused,
                x: 0,
                y: 0,
                rows: self.rows,
                cols: self.cols,
            }];
        }
        let mut rects = Vec::new();
        let mut borders = Vec::new();
        self.layout
            .rects(self.rows, self.cols, &mut rects, &mut borders);
        rects
    }

    fn borders(&self) -> Vec<Border> {
        if self.zoomed {
            return Vec::new();
        }
        let mut rects = Vec::new();
        let mut borders = Vec::new();
        self.layout
            .rects(self.rows, self.cols, &mut rects, &mut borders);
        borders
    }

    pub fn render(&mut self) -> Result<String> {
        let mut output = Vec::new();
        let full_render = self.force_full_render;
        output.write_all(b"\x1b[?25l")?;
        if full_render {
            output.write_all(b"\x1b[H")?;
        }

        for rect in self.rectangles() {
            let rows = {
                let Some(pane) = self.panes.get(&rect.pane_id) else {
                    continue;
                };
                pane.terminal
                    .screen()
                    .rows_formatted(0, rect.cols)
                    .take(usize::from(rect.rows))
                    .collect::<Vec<_>>()
            };
            let previous = self.rendered_rows.get(&rect.pane_id);
            for (row, contents) in rows.iter().enumerate() {
                if full_render
                    || previous.is_none_or(|previous| previous.get(row) != Some(contents))
                {
                    write!(output, "\x1b[{};{}H", rect.y + row as u16 + 1, rect.x + 1)?;
                    output.write_all(contents)?;
                    output.write_all(b"\x1b[K")?;
                }
            }
            self.rendered_rows.insert(rect.pane_id, rows);
        }

        for border in self.borders() {
            for offset in 0..border.length {
                let (row, col, character) = if border.horizontal {
                    (border.y + 1, border.x + offset + 1, b'-')
                } else {
                    (border.y + offset + 1, border.x + 1, b'|')
                };
                write!(output, "\x1b[{};{}H{}", row, col, character as char)?;
            }
        }

        if let Some(rect) = self
            .rectangles()
            .into_iter()
            .find(|rect| rect.pane_id == self.focused)
        {
            if let Some(pane) = self.panes.get(&self.focused) {
                if pane.unread_output > 0 && pane.terminal.is_scrolled() {
                    let label = format!(" {} new lines ", pane.unread_output);
                    let label_width = u16::try_from(label.len()).unwrap_or(rect.cols);
                    if label_width < rect.cols {
                        write!(
                            output,
                            "\x1b[{};{}H\x1b[7m{}\x1b[0m",
                            rect.y + rect.rows,
                            rect.x + rect.cols - label_width + 1,
                            label
                        )?;
                    }
                }
            }
        }

        if let (Some(rect), Some(pane)) = (
            self.rectangles()
                .into_iter()
                .find(|rect| rect.pane_id == self.focused),
            self.panes.get(&self.focused),
        ) {
            output.extend(pane.terminal.screen().input_mode_formatted());
            let (cursor_row, cursor_col) = pane.terminal.screen().cursor_position();
            let cursor_row = cursor_row.min(rect.rows.saturating_sub(1));
            let cursor_col = cursor_col.min(rect.cols.saturating_sub(1));
            if !pane.terminal.screen().hide_cursor() {
                output.write_all(b"\x1b[?25h")?;
            }
            write!(
                output,
                "\x1b[{};{}H",
                rect.y + cursor_row + 1,
                rect.x + cursor_col + 1
            )?;
        }
        if self.mouse_capture {
            output.write_all(b"\x1b[?1000h\x1b[?1006h")?;
        } else {
            output.write_all(b"\x1b[?1000l\x1b[?1006l")?;
        }
        output.write_all(b"\x1b[0m")?;
        self.force_full_render = false;
        Ok(String::from_utf8_lossy(&output).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::{Session, SessionOptions};
    use crate::{config::Config, layout::SplitDirection};

    #[test]
    fn session_splits_and_renders_two_panes() {
        let (events, _) = mpsc::channel();
        let session = Session::new_with_command(
            "test".to_string(),
            1,
            &Config::default(),
            24,
            80,
            SessionOptions {
                command: None,
                temporary: false,
            },
            events,
        )
        .unwrap();
        assert_eq!(session.rectangles().len(), 1);
        let (events, _) = mpsc::channel();
        let mut session = session;
        session
            .split(2, SplitDirection::Vertical, &Config::default(), events)
            .unwrap();
        assert_eq!(session.rectangles().len(), 2);
        let rendered = session.render().unwrap();
        assert!(rendered.contains('|'));
        assert!(!rendered.contains("\x1b[2J"));
        assert!(rendered.contains("\x1b[?25h"));
        let unchanged = session.render().unwrap();
        assert!(!unchanged.contains("\x1b[H"));
        session
            .panes
            .get_mut(&2)
            .unwrap()
            .terminal
            .process(b"\x1b[?25l");
        assert!(!session.render().unwrap().contains("\x1b[?25h"));
        session.adjust_ratio(5).unwrap();
    }

    #[test]
    fn refuses_horizontal_split_below_minimum_height() {
        let (events, _) = mpsc::channel();
        let mut session = Session::new_with_command(
            "small".to_string(),
            1,
            &Config::default(),
            6,
            80,
            SessionOptions {
                command: None,
                temporary: false,
            },
            events,
        )
        .unwrap();
        let (events, _) = mpsc::channel();
        assert!(session
            .split(2, SplitDirection::Horizontal, &Config::default(), events)
            .is_err());
    }

    #[test]
    fn restarts_exited_focused_pane() {
        let (events, _) = mpsc::channel();
        let mut session = Session::new_with_command(
            "restart".to_string(),
            1,
            &Config::default(),
            24,
            80,
            SessionOptions {
                command: None,
                temporary: false,
            },
            events.clone(),
        )
        .unwrap();
        session.record_exit(1, "exit status: 1".to_string());

        assert!(session
            .restart_focused_if_exited(&Config::default(), events)
            .unwrap());
        assert!(!session.exited.contains_key(&1));
        assert!(session.panes.get(&1).unwrap().process_id.is_some());

        let _ = session.panes.get_mut(&1).unwrap().kill();
    }
}
