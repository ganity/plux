use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneRect {
    pub pane_id: u64,
    pub x: u16,
    pub y: u16,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Border {
    pub x: u16,
    pub y: u16,
    pub horizontal: bool,
    pub length: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum LayoutNode {
    Leaf(u64),
    Split {
        direction: SplitDirection,
        ratio_percent: u16,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

impl LayoutNode {
    pub fn leaf(pane_id: u64) -> Self {
        Self::Leaf(pane_id)
    }

    pub fn split_leaf(
        &mut self,
        pane_id: u64,
        new_pane_id: u64,
        direction: SplitDirection,
    ) -> bool {
        match self {
            Self::Leaf(id) if *id == pane_id => {
                *self = Self::Split {
                    direction,
                    ratio_percent: 50,
                    first: Box::new(Self::Leaf(pane_id)),
                    second: Box::new(Self::Leaf(new_pane_id)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.split_leaf(pane_id, new_pane_id, direction)
                    || second.split_leaf(pane_id, new_pane_id, direction)
            }
        }
    }

    pub fn close_leaf(&mut self, pane_id: u64) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                if first.contains(pane_id) {
                    if matches!(**first, Self::Leaf(_)) {
                        *self = (**second).clone();
                        true
                    } else {
                        first.close_leaf(pane_id)
                    }
                } else if second.contains(pane_id) {
                    if matches!(**second, Self::Leaf(_)) {
                        *self = (**first).clone();
                        true
                    } else {
                        second.close_leaf(pane_id)
                    }
                } else {
                    false
                }
            }
        }
    }

    pub fn adjust_ratio_for(&mut self, pane_id: u64, delta: i16) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split {
                ratio_percent,
                first,
                second,
                ..
            } => {
                if first.contains(pane_id) && first.adjust_ratio_for(pane_id, delta) {
                    return true;
                }
                if second.contains(pane_id) && second.adjust_ratio_for(pane_id, delta) {
                    return true;
                }
                if first.contains(pane_id) || second.contains(pane_id) {
                    *ratio_percent =
                        (i32::from(*ratio_percent) + i32::from(delta)).clamp(10, 90) as u16;
                    return true;
                }
                false
            }
        }
    }

    pub fn contains(&self, pane_id: u64) -> bool {
        match self {
            Self::Leaf(id) => *id == pane_id,
            Self::Split { first, second, .. } => {
                first.contains(pane_id) || second.contains(pane_id)
            }
        }
    }

    pub fn pane_ids(&self, ids: &mut Vec<u64>) {
        match self {
            Self::Leaf(id) => ids.push(*id),
            Self::Split { first, second, .. } => {
                first.pane_ids(ids);
                second.pane_ids(ids);
            }
        }
    }

    pub fn rects(
        &self,
        rows: u16,
        cols: u16,
        rects: &mut Vec<PaneRect>,
        borders: &mut Vec<Border>,
    ) {
        self.collect_rects(0, 0, rows, cols, rects, borders);
    }

    fn collect_rects(
        &self,
        x: u16,
        y: u16,
        rows: u16,
        cols: u16,
        rects: &mut Vec<PaneRect>,
        borders: &mut Vec<Border>,
    ) {
        match self {
            Self::Leaf(pane_id) => rects.push(PaneRect {
                pane_id: *pane_id,
                x,
                y,
                rows: rows.max(1),
                cols: cols.max(1),
            }),
            Self::Split {
                direction,
                ratio_percent,
                first,
                second,
            } => match direction {
                SplitDirection::Horizontal => {
                    let available = rows.saturating_sub(1).max(2);
                    let first_rows = (available * *ratio_percent / 100).clamp(1, available - 1);
                    let second_rows = available - first_rows;
                    first.collect_rects(x, y, first_rows, cols, rects, borders);
                    borders.push(Border {
                        x,
                        y: y + first_rows,
                        horizontal: true,
                        length: cols,
                    });
                    second.collect_rects(x, y + first_rows + 1, second_rows, cols, rects, borders);
                }
                SplitDirection::Vertical => {
                    let available = cols.saturating_sub(1).max(2);
                    let first_cols = (available * *ratio_percent / 100).clamp(1, available - 1);
                    let second_cols = available - first_cols;
                    first.collect_rects(x, y, rows, first_cols, rects, borders);
                    borders.push(Border {
                        x: x + first_cols,
                        y,
                        horizontal: false,
                        length: rows,
                    });
                    second.collect_rects(x + first_cols + 1, y, rows, second_cols, rects, borders);
                }
            },
        }
    }

    pub fn focus_neighbor(
        &self,
        current: u64,
        direction: FocusDirection,
        rows: u16,
        cols: u16,
    ) -> Option<u64> {
        let mut rects = Vec::new();
        let mut borders = Vec::new();
        self.rects(rows, cols, &mut rects, &mut borders);
        let current_rect = *rects.iter().find(|rect| rect.pane_id == current)?;
        let current_center_x = current_rect.x + current_rect.cols / 2;
        let current_center_y = current_rect.y + current_rect.rows / 2;

        rects
            .into_iter()
            .filter(|rect| rect.pane_id != current)
            .filter(|rect| match direction {
                FocusDirection::Left => rect.x + rect.cols <= current_rect.x,
                FocusDirection::Right => rect.x >= current_rect.x + current_rect.cols,
                FocusDirection::Up => rect.y + rect.rows <= current_rect.y,
                FocusDirection::Down => rect.y >= current_rect.y + current_rect.rows,
            })
            .min_by_key(|rect| {
                let center_x = rect.x + rect.cols / 2;
                let center_y = rect.y + rect.rows / 2;
                match direction {
                    FocusDirection::Left | FocusDirection::Right => (
                        current_center_x.abs_diff(center_x),
                        current_center_y.abs_diff(center_y),
                    ),
                    FocusDirection::Up | FocusDirection::Down => (
                        current_center_y.abs_diff(center_y),
                        current_center_x.abs_diff(center_x),
                    ),
                }
            })
            .map(|rect| rect.pane_id)
    }
}

#[cfg(test)]
mod tests {
    use super::{FocusDirection, LayoutNode, SplitDirection};

    #[test]
    fn splits_and_closes_leaf() {
        let mut layout = LayoutNode::leaf(1);
        assert!(layout.split_leaf(1, 2, SplitDirection::Vertical));
        assert_eq!(layout.pane_ids_vec(), vec![1, 2]);
        assert!(layout.close_leaf(2));
        assert_eq!(layout.pane_ids_vec(), vec![1]);
    }

    #[test]
    fn finds_neighbor() {
        let mut layout = LayoutNode::leaf(1);
        layout.split_leaf(1, 2, SplitDirection::Vertical);
        assert_eq!(
            layout.focus_neighbor(1, FocusDirection::Right, 24, 80),
            Some(2)
        );
    }

    trait PaneIds {
        fn pane_ids_vec(&self) -> Vec<u64>;
    }

    impl PaneIds for LayoutNode {
        fn pane_ids_vec(&self) -> Vec<u64> {
            let mut ids = Vec::new();
            self.pane_ids(&mut ids);
            ids
        }
    }
}
