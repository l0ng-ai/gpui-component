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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineEdit {
    pub start_line: usize,
    pub end_line: usize,
    pub new_lines: usize,
}

impl LineEdit {
    /// Where line `line` (0-based, before the edit) is after it. A line the
    /// edit swallowed lands on the last line of what replaced it.
    pub fn shift(&self, line: usize) -> usize {
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
}

impl InputState {
    pub(super) fn log_line_edit(&mut self, old_text: &Rope, range: &Range<usize>, new_text: &str) {
        let log = &mut self.edit_log;
        log.version += 1;
        log.edits.push_back(LineEdit {
            start_line: old_text.offset_to_point(range.start).row,
            end_line: old_text.offset_to_point(range.end.min(old_text.len())).row,
            new_lines: new_text.bytes().filter(|b| *b == b'\n').count(),
        });
        if log.edits.len() > EDIT_LOG_LEN {
            log.edits.pop_front();
        }
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

#[cfg(test)]
mod tests {
    use super::LineEdit;

    #[test]
    fn lines_below_an_edit_move_with_it_and_lines_above_stay() {
        // Two lines inserted after line 5.
        let insert = LineEdit {
            start_line: 5,
            end_line: 5,
            new_lines: 2,
        };
        assert_eq!(insert.shift(3), 3);
        assert_eq!(insert.shift(5), 5);
        assert_eq!(insert.shift(9), 11);
        // Lines 4..=8 collapsed into one.
        let delete = LineEdit {
            start_line: 4,
            end_line: 8,
            new_lines: 0,
        };
        assert_eq!(delete.shift(2), 2);
        assert_eq!(delete.shift(6), 4);
        assert_eq!(delete.shift(20), 16);
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
                        new_lines: 2
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
