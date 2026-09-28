//! A short record of which lines each edit touched, for hosts that keep
//! line numbers of their own into the buffer — a navigation history, a list
//! of bookmarks — and need to move them along with the text.
//!
//! The record is bounded: a host that falls more than [`EDIT_LOG_LEN`] edits
//! behind is told so (`None`) rather than handed a partial answer.

use std::collections::VecDeque;
use std::ops::Range;

use ropey::Rope;

use super::{InputState, RopeExt as _};

/// How many edits the log keeps.
pub const EDIT_LOG_LEN: usize = 256;

/// One edit, in lines: buffer lines `start_line..=end_line` (at least partly)
/// were replaced by text spanning `new_lines + 1` lines.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineEdit {
    pub start_line: usize,
    pub end_line: usize,
    pub new_lines: usize,
    /// The edit only inserted, at column 0 of `start_line`: nothing of that
    /// line was touched, so the whole of it moves down with the lines after
    /// it — Enter at the start of a line pushes the line down, it doesn't
    /// leave it where it was.
    pub at_line_start: bool,
}

impl LineEdit {
    /// The line edit replacing `range` of `old_text` with `new_text` makes.
    pub fn new(old_text: &Rope, range: &Range<usize>, new_text: &str) -> Self {
        let start = old_text.offset_to_point(range.start);
        Self {
            start_line: start.row,
            end_line: old_text.offset_to_point(range.end.min(old_text.len())).row,
            new_lines: new_text.bytes().filter(|b| *b == b'\n').count(),
            at_line_start: range.is_empty() && start.column == 0,
        }
    }

    /// Where line `line` (0-based, before the edit) is after it. A line the
    /// edit swallowed lands on the last line of what replaced it.
    pub fn shift(&self, line: usize) -> usize {
        if self.at_line_start && line >= self.start_line {
            return line + self.new_lines;
        }
        if line > self.end_line {
            (line as isize + self.new_lines as isize - (self.end_line - self.start_line) as isize)
                .max(0) as usize
        } else if line > self.start_line {
            line.min(self.start_line + self.new_lines)
        } else {
            line
        }
    }
}

#[derive(Default)]
pub(super) struct EditLog {
    /// How many edits the buffer has had, ever.
    version: u64,
    /// The last few, oldest first; the newest is `version`.
    edits: VecDeque<LineEdit>,
    /// The same edits in bytes — `(replaced range, inserted len)` — for
    /// [`InputState::map_range_since`].
    byte_edits: VecDeque<(Range<usize>, usize)>,
}

impl InputState {
    pub(super) fn log_line_edit(&mut self, old_text: &Rope, range: &Range<usize>, new_text: &str) {
        let log = &mut self.edit_log;
        log.version += 1;
        log.edits.push_back(LineEdit::new(old_text, range, new_text));
        log.byte_edits.push_back((range.clone(), new_text.len()));
        if log.edits.len() > EDIT_LOG_LEN {
            log.edits.pop_front();
            log.byte_edits.pop_front();
        }
    }

    /// Carry `range`, measured against the text at `version`, through the
    /// edits made since: its start stays before text inserted right at it,
    /// its end takes such text in — so a word being completed keeps the
    /// characters typed after the request went out. `None` when the log has
    /// forgotten some of those edits.
    pub(crate) fn map_range_since(&self, version: u64, range: Range<usize>) -> Option<Range<usize>> {
        let log = &self.edit_log;
        let behind = log.version.checked_sub(version)? as usize;
        if behind > log.byte_edits.len() {
            return None;
        }
        let edits = log.byte_edits.iter().skip(log.byte_edits.len() - behind);
        Some(edits.fold(range, |range, (edit, new_len)| {
            map_range_through(range, edit, *new_len)
        }))
    }

    /// How many edits the text has had. Pass it back to
    /// [`Self::line_edits_since`] to hear about the ones after it.
    pub fn edit_version(&self) -> u64 {
        self.edit_log.version
    }

    /// The edits made since `version`, oldest first. `None` when there have
    /// been too many to remember them all.
    pub fn line_edits_since(&self, version: u64) -> Option<Vec<LineEdit>> {
        let log = &self.edit_log;
        let behind = log.version.checked_sub(version)? as usize;
        if behind > log.edits.len() {
            return None;
        }
        Some(
            log.edits
                .iter()
                .skip(log.edits.len() - behind)
                .copied()
                .collect(),
        )
    }
}

/// Map `range` through an edit that replaced `edit` with `new_len` bytes:
/// the start sticks left, the end right.
pub(crate) fn map_range_through(
    range: Range<usize>,
    edit: &Range<usize>,
    new_len: usize,
) -> Range<usize> {
    let shift = |p: usize| p - edit.end + edit.start + new_len;
    let start = if range.start <= edit.start {
        range.start
    } else if range.start >= edit.end {
        shift(range.start)
    } else {
        edit.start
    };
    let end = if range.end < edit.start {
        range.end
    } else if range.end >= edit.end {
        shift(range.end)
    } else {
        edit.start + new_len
    };
    start..end.max(start)
}

#[cfg(test)]
mod tests {
    use super::{LineEdit, map_range_through};
    use ropey::Rope;

    #[test]
    fn lines_below_an_edit_move_with_it_and_lines_above_stay() {
        // Two lines inserted after line 5.
        let insert = LineEdit {
            start_line: 5,
            end_line: 5,
            new_lines: 2,
            at_line_start: false,
        };
        assert_eq!(insert.shift(3), 3);
        assert_eq!(insert.shift(5), 5);
        assert_eq!(insert.shift(9), 11);
        // Lines 4..=8 collapsed into one.
        let delete = LineEdit {
            start_line: 4,
            end_line: 8,
            new_lines: 0,
            at_line_start: false,
        };
        assert_eq!(delete.shift(2), 2);
        assert_eq!(delete.shift(6), 4);
        assert_eq!(delete.shift(20), 16);
    }

    #[test]
    fn enter_at_column_zero_pushes_the_line_down() {
        let text = Rope::from("a\nbb\ncc\n");
        // Enter at the start of line 1.
        let enter = LineEdit::new(&text, &(2..2), "\n");
        assert!(enter.at_line_start);
        assert_eq!(enter.shift(0), 0);
        assert_eq!(enter.shift(1), 2);
        assert_eq!(enter.shift(2), 3);
        // At the very top too.
        let top = LineEdit::new(&text, &(0..0), "use x;\n\n");
        assert_eq!(top.shift(0), 2);
        // Enter mid-line leaves the line where it starts.
        let mid = LineEdit::new(&text, &(3..3), "\n");
        assert!(!mid.at_line_start);
        assert_eq!(mid.shift(1), 1);
        assert_eq!(mid.shift(2), 3);
        // A replacement from column 0 does touch the line.
        let replace = LineEdit::new(&text, &(2..3), "x\ny");
        assert!(!replace.at_line_start);
    }

    #[test]
    fn a_range_follows_the_edits_around_it() {
        // The word `ab` at 4..6; `c` typed after it.
        assert_eq!(map_range_through(4..6, &(6..6), 1), 4..7);
        // Something inserted before it.
        assert_eq!(map_range_through(4..6, &(0..0), 3), 7..9);
        // Text inserted right at its start joins it: the start stays put.
        assert_eq!(map_range_through(4..6, &(4..4), 2), 4..8);
        // …and inside grows it.
        assert_eq!(map_range_through(4..6, &(5..5), 2), 4..8);
        // Deleted around: collapses.
        assert_eq!(map_range_through(4..6, &(2..8), 0), 2..2);
    }

    #[gpui::test]
    fn the_log_answers_since_a_version_until_it_has_forgotten(cx: &mut gpui::TestAppContext) {
        use super::super::InputState;
        use gpui::{AppContext as _, EntityInputHandler as _, VisualTestContext};

        let mut input = None;
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                cx.set_global(crate::theme::Theme::default());
                crate::input::init(cx);
                let state = cx.new(|cx| {
                    InputState::new(window, cx)
                        .code_editor("text")
                        .default_value("a\nb\nc\nd\n")
                });
                input = Some(state.clone());
                cx.new(|cx| crate::Root::new(state, window, cx))
            })
            .unwrap()
        });
        let input = input.unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                let v = state.edit_version();
                state.replace_text_in_range(Some(0..0), "x\ny\n", window, cx);
                assert_eq!(
                    state.line_edits_since(v),
                    Some(vec![LineEdit {
                        start_line: 0,
                        end_line: 0,
                        new_lines: 2,
                        at_line_start: true,
                    }])
                );
                assert_eq!(state.line_edits_since(state.edit_version()), Some(vec![]));
                for _ in 0..super::EDIT_LOG_LEN {
                    state.replace_text_in_range(Some(0..0), "z", window, cx);
                }
                assert_eq!(state.line_edits_since(v), None, "too far behind");
            });
        });
    }
}
