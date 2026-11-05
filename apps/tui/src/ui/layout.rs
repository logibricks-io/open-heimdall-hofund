//! List window calculations (cursor stays visible).

/// Computes which rows to show in a list body.
#[must_use]
pub fn list_window(body_height: u16, min_visible: u16, row_count: usize, cursor: usize) -> Window {
    let visible = min_visible.max(body_height.saturating_sub(1)) as usize;
    if row_count == 0 {
        return Window {
            start: 0,
            visible: visible.min(1),
        };
    }
    let visible = visible.min(row_count);
    let mut start = 0;
    if cursor >= visible {
        start = cursor - visible + 1;
    }
    let start = start.min(row_count.saturating_sub(visible));
    Window { start, visible }
}

/// A visible slice of list rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// First row index in the full list.
    pub start: usize,
    /// Number of rows to draw.
    pub visible: usize,
}

#[cfg(test)]
mod tests {
    use super::list_window;

    #[test]
    fn resize_keeps_cursor_on_screen() {
        let w = list_window(5, 3, 20, 15);
        assert!(w.start <= 15);
        assert!(w.start + w.visible > 15);
    }

    #[test]
    fn library_uses_at_least_four_rows_when_tall_enough() {
        let w = list_window(20, 4, 10, 0);
        assert!(w.visible >= 4);
    }
}
