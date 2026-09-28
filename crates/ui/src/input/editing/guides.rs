//! The active indent guide: the one line of the scope the caret is in, drawn
//! a little stronger over the regular guides (`indent.rs`).
use std::ops::Range;

use gpui::{
    Bounds, Hsla, Path, PathBuilder, Pixels, SharedString, TextRun, TextStyle, Window, point, px,
};
use ropey::Rope;

use super::{indent_columns, is_blank, line_string};
use crate::input::{InputState, LastLayout, RopeExt as _, TabSize, element::TextElement};

/// How far the scope search walks from the caret, in lines.
const MAX_SCOPE_LINES: usize = 5000;

fn level(text: &Rope, row: usize, tab: TabSize) -> Option<usize> {
    let line = line_string(text, row);
    if is_blank(&line) {
        return None;
    }
    Some(indent_columns(&line, tab) / tab.tab_size.max(1))
}

/// The indent level of `row`, with a blank line taking the deeper of its
/// nearest non-blank neighbours.
fn effective_level(text: &Rope, row: usize, tab: TabSize) -> usize {
    if let Some(level) = level(text, row, tab) {
        return level;
    }
    let total = text.lines_len();
    let above = (row.saturating_sub(MAX_SCOPE_LINES)..row)
        .rev()
        .find_map(|r| level(text, r, tab));
    let below = (row + 1..total.min(row + MAX_SCOPE_LINES)).find_map(|r| level(text, r, tab));
    above.unwrap_or(0).max(below.unwrap_or(0))
}

/// The guide the caret's scope hangs from: its level (the guide at column
/// `level * tab_size`) and the rows it runs over.
///
/// On a line that opens a block (the next non-blank line is deeper) it is the
/// guide of that block; otherwise the guide of the block the line is in.
pub(crate) fn active_indent_guide(
    text: &Rope,
    row: usize,
    tab: TabSize,
) -> Option<(usize, Range<usize>)> {
    let total = text.lines_len();
    if row >= total {
        return None;
    }
    let here = effective_level(text, row, tab);
    let next = (row + 1..total.min(row + MAX_SCOPE_LINES)).find_map(|r| level(text, r, tab));

    let (guide, start) = match next {
        Some(next) if next > here => (here, row + 1),
        _ if here == 0 => return None,
        _ => (here - 1, row),
    };
    let inside = |r: usize| level(text, r, tab).is_none_or(|l| l > guide);

    let mut top = start;
    while top > 0 && start - top < MAX_SCOPE_LINES && inside(top - 1) {
        top -= 1;
    }
    let mut bottom = start;
    while bottom + 1 < total && bottom - start < MAX_SCOPE_LINES && inside(bottom + 1) {
        bottom += 1;
    }
    // Blank lines at either end belong to no scope.
    while top < bottom && level(text, top, tab).is_none() {
        top += 1;
    }
    while bottom > top && level(text, bottom, tab).is_none() {
        bottom -= 1;
    }
    if level(text, top, tab).is_none() {
        return None;
    }
    Some((guide, top..bottom + 1))
}

impl TextElement {
    fn indent_unit_width(style: &TextStyle, columns: usize, window: &Window) -> Pixels {
        let font_size = style.font_size.to_pixels(window.rem_size());
        window
            .text_system()
            .shape_line(
                SharedString::from(" ".repeat(columns)),
                font_size,
                &[TextRun {
                    len: columns,
                    font: style.font(),
                    color: Hsla::default(),
                    background_color: None,
                    strikethrough: None,
                    underline: None,
                }],
                None,
            )
            .width
    }

    /// The path of the active indent guide over the visible lines.
    pub(in crate::input) fn layout_active_indent_guide(
        &self,
        state: &InputState,
        bounds: &Bounds<Pixels>,
        last_layout: &LastLayout,
        text_style: &TextStyle,
        window: &mut Window,
    ) -> Option<Path<Pixels>> {
        if !state.mode.has_indent_guides() || !state.focus_handle.is_focused(window) {
            return None;
        }
        let tab = state.mode.tab_size();
        let row = state.text.offset_to_point(state.cursor()).row;
        let (guide, rows) = active_indent_guide(&state.text, row, tab)?;

        let indent_width = Self::indent_unit_width(text_style, tab.tab_size, window);
        let x = indent_width * guide as f32 + last_layout.line_number_width;
        let line_height = last_layout.line_height;
        let mut builder = PathBuilder::stroke(px(1.));
        let mut offset_y = last_layout.visible_top;
        let mut any = false;
        for (&buffer_line, line_layout) in last_layout
            .visible_buffer_lines
            .iter()
            .zip(last_layout.lines.iter())
        {
            let height = line_layout.wrapped_lines.len() * line_height;
            if rows.contains(&buffer_line) {
                builder.move_to(point(x, offset_y));
                builder.line_to(point(x, offset_y + height));
                any = true;
            }
            offset_y += height;
        }
        if !any {
            return None;
        }
        builder.translate(bounds.origin);
        builder.build().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAB: TabSize = TabSize {
        tab_size: 2,
        hard_tabs: false,
    };

    fn guide(text: &str, row: usize) -> Option<(usize, Range<usize>)> {
        active_indent_guide(&Rope::from(text), row, TAB)
    }

    #[test]
    fn test_active_indent_guide() {
        let text = "fn a() {\n  let x;\n  if y {\n    z();\n  }\n}\n";
        // Inside the function body: the level-0 guide over the body.
        assert_eq!(guide(text, 1), Some((0, 1..5)));
        // On the line that opens the `if`: the `if` block's guide.
        assert_eq!(guide(text, 2), Some((1, 3..4)));
        // Inside the `if`.
        assert_eq!(guide(text, 3), Some((1, 3..4)));
        // On the function line: its body.
        assert_eq!(guide(text, 0), Some((0, 1..5)));
        // At the top level with nothing below: none.
        assert_eq!(guide(text, 5), None);
    }

    #[test]
    fn test_active_indent_guide_blank_lines() {
        let text = "a:\n  b\n\n  c\n\nd";
        assert_eq!(guide(text, 2), Some((0, 1..4)));
        assert_eq!(guide(text, 1), Some((0, 1..4)));
        // A blank line takes the deeper of its neighbours, so it is still in
        // the block above it.
        assert_eq!(guide(text, 4), Some((0, 1..4)));
        // A top-level line with nothing indented after it has none.
        assert_eq!(guide(text, 5), None);
    }
}
