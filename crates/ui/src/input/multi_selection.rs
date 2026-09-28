//! Multiple cursors / selections for the code editor.
//!
//! The primary selection stays where it always was — `selected_range`,
//! `selection_reversed`, `preferred_column` and `ime_marked_range` on
//! [`InputState`] — so every single-selection command keeps working unchanged.
//! Any further cursors live in `extra_selections`, and
//! [`InputState::edit_each_selection`] runs a single-selection command once
//! per selection, as one undo step, shifting the other selections past the
//! edits each pass made.
//!
//! The primary selection is always the most recently added one (the one
//! ⌘D / ⌥-click / ⌥⌘↓ just created), which is where the IME, completion,
//! scroll-into-view and the status bar all look.
use std::ops::Range;

use gpui::{
    App, ClipboardItem, Context, Entity, KeyBinding, MouseButton, MouseDownEvent, Pixels, Window,
    actions,
};
use ropey::Rope;

use super::{
    Selection,
    movement::MoveDirection,
    state::{CONTEXT, InputState},
};
use crate::input::RopeExt as _;

actions!(
    input,
    [
        /// Select the word under the cursor, or add the next occurrence of
        /// the current selection as a new selection (VS Code's ⌘D).
        SelectNextOccurrence,
        /// Select every occurrence of the current selection (VS Code's ⌘⇧L).
        SelectAllOccurrences,
        /// Add a cursor on the line above the topmost cursor.
        AddCursorAbove,
        /// Add a cursor on the line below the bottommost cursor.
        AddCursorBelow,
    ]
);

pub(super) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-d", SelectNextOccurrence, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-l", SelectAllOccurrences, Some(CONTEXT)),
        KeyBinding::new("secondary-alt-up", AddCursorAbove, Some(CONTEXT)),
        KeyBinding::new("secondary-alt-down", AddCursorBelow, Some(CONTEXT)),
    ]);
}

/// A selection other than the primary one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ExtraSelection {
    pub(crate) range: Selection,
    /// The head (caret) is at `range.start` rather than `range.end`.
    pub(crate) reversed: bool,
    /// This selection's own `preferred_column`, for Up/Down.
    pub(crate) preferred_column: Option<(Pixels, usize)>,
}

impl ExtraSelection {
    pub(crate) fn new(range: impl Into<Selection>, reversed: bool) -> Self {
        Self {
            range: range.into(),
            reversed,
            preferred_column: None,
        }
    }

    /// The caret offset.
    pub(crate) fn head(&self) -> usize {
        if self.reversed {
            self.range.start
        } else {
            self.range.end
        }
    }
}

/// `window.listener_for(state, handler)`, with `handler` run once per
/// selection — see [`InputState::each_selection`].
pub(super) fn each_selection<A: 'static>(
    window: &Window,
    state: &Entity<InputState>,
    handler: fn(&mut InputState, &A, &mut Window, &mut Context<InputState>),
) -> impl Fn(&A, &mut Window, &mut App) + 'static {
    window.listener_for(state, InputState::each_selection(handler))
}

/// Bookkeeping for one [`InputState::edit_each_selection`] run.
#[derive(Debug, Default)]
pub(crate) struct MultiEdit {
    /// The pass currently running is the primary selection's.
    pub(crate) is_primary: bool,
    /// Every edit the current pass made, as `(replaced range, inserted len)`
    /// in the order they were applied.
    pub(crate) edits: Vec<(Range<usize>, usize)>,
}

/// Map a byte offset through an edit that replaced `range` with `new_len`
/// bytes. Offsets before the edit stay, offsets after it shift, and an offset
/// inside the replaced text lands at most at the end of the new text.
pub(crate) fn map_offset(offset: usize, range: &Range<usize>, new_len: usize) -> usize {
    if offset <= range.start {
        offset
    } else if offset >= range.end {
        offset - range.len() + new_len
    } else {
        offset.min(range.start + new_len)
    }
}

fn map_selection(selection: &mut ExtraSelection, edits: &[(Range<usize>, usize)]) {
    for (range, new_len) in edits {
        selection.range.start = map_offset(selection.range.start, range, *new_len);
        selection.range.end = map_offset(selection.range.end, range, *new_len);
    }
}

/// Sort the selections and merge the ones that overlap (or that touch, when
/// one of them is a bare caret). Returns the merged list and the new index of
/// `primary`.
pub(crate) fn merge_selections(
    selections: Vec<ExtraSelection>,
    primary: usize,
) -> (Vec<ExtraSelection>, usize) {
    let mut items: Vec<(ExtraSelection, bool)> = selections
        .into_iter()
        .enumerate()
        .map(|(ix, s)| (s, ix == primary))
        .collect();
    items.sort_by_key(|(s, _)| (s.range.start, s.range.end));

    let mut merged: Vec<(ExtraSelection, bool)> = Vec::with_capacity(items.len());
    for (sel, is_primary) in items {
        if let Some((last, last_primary)) = merged.last_mut() {
            let overlaps = sel.range.start < last.range.end
                || (sel.range.start == last.range.end
                    && (sel.range.is_empty() || last.range.is_empty()));
            if overlaps {
                let end = last.range.end.max(sel.range.end);
                if is_primary {
                    last.reversed = sel.reversed;
                    last.preferred_column = sel.preferred_column;
                }
                last.range.end = end;
                *last_primary |= is_primary;
                continue;
            }
        }
        merged.push((sel, is_primary));
    }

    let primary = merged.iter().position(|(_, p)| *p).unwrap_or(0);
    (merged.into_iter().map(|(s, _)| s).collect(), primary)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `text[range]` stands alone as a word: the characters around it aren't
/// word characters.
fn is_whole_word(text: &str, range: &Range<usize>) -> bool {
    let before = text[..range.start].chars().next_back();
    let after = text[range.end..].chars().next();
    !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
}

/// Every non-overlapping occurrence of `needle` in `text`.
pub(crate) fn find_occurrences(text: &str, needle: &str, whole_word: bool) -> Vec<Range<usize>> {
    if needle.is_empty() {
        return vec![];
    }
    text.match_indices(needle)
        .map(|(ix, m)| ix..ix + m.len())
        .filter(|range| !whole_word || is_whole_word(text, range))
        .collect()
}

/// The first occurrence of `needle` at or after `from`, wrapping around to
/// the start of the text, that isn't one of `taken`.
pub(crate) fn find_next_occurrence(
    text: &str,
    needle: &str,
    from: usize,
    whole_word: bool,
    taken: &[Range<usize>],
) -> Option<Range<usize>> {
    let all = find_occurrences(text, needle, whole_word);
    let split = all.partition_point(|r| r.start < from);
    all[split..]
        .iter()
        .chain(all[..split].iter())
        .find(|r| !taken.contains(r))
        .cloned()
}

/// Split a paste into one piece per selection, the way VS Code does: only
/// when the clipboard holds exactly as many lines as there are selections.
pub(crate) fn distribute_paste(text: &str, count: usize) -> Option<Vec<String>> {
    if count < 2 {
        return None;
    }
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect();
    (lines.len() == count).then_some(lines)
}

/// The word under (or just before) `offset`, if there is one.
fn word_at(text: &Rope, offset: usize) -> Option<Range<usize>> {
    let is_word = |range: &Range<usize>| {
        text.slice(range.clone())
            .chars()
            .next()
            .is_some_and(is_word_char)
    };
    if let Some(range) = super::selection::TextSelector::word_range(text, offset)
        && is_word(&range)
    {
        return Some(range);
    }
    let prev = text.clip_offset(offset.saturating_sub(1), sum_tree::Bias::Left);
    if prev < offset
        && let Some(range) = super::selection::TextSelector::word_range(text, prev)
        && is_word(&range)
    {
        return Some(range);
    }
    None
}

impl InputState {
    /// Whether there is more than one cursor.
    pub fn has_extra_selections(&self) -> bool {
        !self.extra_selections.is_empty()
    }

    /// The number of cursors / selections, the primary one included.
    pub fn selection_count(&self) -> usize {
        1 + self.extra_selections.len()
    }

    /// Every selection as a byte range, in document order.
    pub fn selected_ranges(&self) -> Vec<Range<usize>> {
        let mut ranges: Vec<Range<usize>> = std::iter::once(self.selected_range.into())
            .chain(self.extra_selections.iter().map(|s| s.range.into()))
            .collect();
        ranges.sort_by_key(|r| (r.start, r.end));
        ranges
    }

    /// Replace every selection. `ranges` are byte ranges with the caret at
    /// the end; the last one becomes the primary selection. Overlapping ranges
    /// are merged, and an empty list is ignored.
    pub fn set_selected_ranges(
        &mut self,
        ranges: impl IntoIterator<Item = Range<usize>>,
        cx: &mut Context<Self>,
    ) {
        let len = self.text.len();
        let selections: Vec<ExtraSelection> = ranges
            .into_iter()
            .map(|r| {
                let start = self
                    .text
                    .clip_offset(r.start.min(len), sum_tree::Bias::Left);
                let end = self.text.clip_offset(r.end.min(len), sum_tree::Bias::Right);
                ExtraSelection::new(start.min(end)..end.max(start), false)
            })
            .collect();
        if selections.is_empty() {
            return;
        }
        let primary = selections.len() - 1;
        self.set_all_selections(selections, primary);
        self.update_preferred_column();
        cx.notify();
    }

    /// Drop every cursor but the primary one. Returns `true` if there were
    /// any to drop.
    pub fn collapse_selections(&mut self, cx: &mut Context<Self>) -> bool {
        if self.extra_selections.is_empty() {
            return false;
        }
        self.extra_selections.clear();
        cx.notify();
        true
    }

    /// Forget the extra cursors without a repaint; for paths that notify
    /// anyway (a click, `set_value`, …).
    pub(super) fn clear_extra_selections(&mut self) {
        self.extra_selections.clear();
    }

    /// Run `f` once for every selection, as one undo step.
    ///
    /// This is how a command written for the single `selected_range` becomes
    /// multi-cursor aware: before each pass the selection is swapped into
    /// `selected_range` / `selection_reversed` / `preferred_column`, `f` does
    /// whatever it does to it (edit through `replace_text_in_range*`, move,
    /// extend), and afterwards the resulting `selected_range` is read back.
    /// Edits a pass makes shift every other selection, so `f` may edit
    /// anywhere — indenting a line start before another caret on the same line
    /// included. Passes run from the last selection to the first, and
    /// selections that end up overlapping are merged.
    ///
    /// With a single cursor this is exactly `f(self, window, cx)`.
    ///
    /// Only the primary pass sees the IME marked range, and scroll requests
    /// from the other passes are dropped, so the view follows the primary
    /// caret. LSP completion isn't triggered by the edits made here.
    pub fn edit_each_selection(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        mut f: impl FnMut(&mut Self, &mut Window, &mut Context<Self>),
    ) {
        self.edit_each_selection_indexed(window, cx, |this, _, window, cx| f(this, window, cx));
    }

    /// [`Self::edit_each_selection`], with the selection's index in document
    /// order passed to `f`.
    pub fn edit_each_selection_indexed(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        mut f: impl FnMut(&mut Self, usize, &mut Window, &mut Context<Self>),
    ) {
        if self.extra_selections.is_empty() || self.multi_edit.is_some() {
            f(self, 0, window, cx);
            return;
        }

        let (mut selections, primary) = self.take_all_selections();
        let mut ime = self.ime_marked_range.take();
        self.multi_edit = Some(MultiEdit::default());

        for ix in (0..selections.len()).rev() {
            let is_primary = ix == primary;
            let sel = selections[ix];
            self.selected_range = sel.range;
            self.selection_reversed = sel.reversed;
            self.preferred_column = sel.preferred_column;
            self.ime_marked_range = if is_primary { ime } else { None };
            // Carets made by ⌘D / ⌥-click haven't got a column yet; Up/Down
            // would otherwise snap them to column 0.
            if self.preferred_column.is_none() {
                self.update_preferred_column();
            }
            if let Some(multi_edit) = self.multi_edit.as_mut() {
                multi_edit.is_primary = is_primary;
                multi_edit.edits.clear();
            }
            let deferred_scroll = self.deferred_scroll_offset;

            f(self, ix, window, cx);

            if !is_primary {
                self.deferred_scroll_offset = deferred_scroll;
            }
            let edits = self
                .multi_edit
                .as_mut()
                .map(|m| std::mem::take(&mut m.edits))
                .unwrap_or_default();
            for (jx, other) in selections.iter_mut().enumerate() {
                if jx != ix {
                    map_selection(other, &edits);
                }
            }
            if is_primary {
                ime = self.ime_marked_range;
            } else if let Some(ime) = ime.as_mut() {
                let mut mapped = ExtraSelection::new(*ime, false);
                map_selection(&mut mapped, &edits);
                *ime = mapped.range;
            }
            selections[ix] = ExtraSelection {
                range: self.selected_range,
                reversed: self.selection_reversed,
                preferred_column: self.preferred_column,
            };
        }

        self.multi_edit = None;
        self.history.end_grouping();
        self.set_all_selections(selections, primary);
        self.ime_marked_range = ime;
        cx.notify();
    }

    /// Every selection, primary included, sorted, with the primary's index.
    fn take_all_selections(&mut self) -> (Vec<ExtraSelection>, usize) {
        let mut all = std::mem::take(&mut self.extra_selections);
        all.push(ExtraSelection {
            range: self.selected_range,
            reversed: self.selection_reversed,
            preferred_column: self.preferred_column,
        });
        let primary = all.len() - 1;
        merge_selections(all, primary)
    }

    /// Store `selections` back, merged, `primary` into `selected_range`.
    fn set_all_selections(&mut self, selections: Vec<ExtraSelection>, primary: usize) {
        let (mut selections, primary) = merge_selections(selections, primary);
        let main = selections.remove(primary);
        self.selected_range = main.range;
        self.selection_reversed = main.reversed && !main.range.is_empty();
        self.preferred_column = main.preferred_column;
        self.extra_selections = selections;
    }

    /// Called for every edit `replace_text_in_range*` applies: inside
    /// [`Self::edit_each_selection`] the edit is recorded for the run to shift
    /// the other selections by, outside it the extra selections shift now.
    pub(super) fn record_edit_for_selections(&mut self, range: &Range<usize>, new_len: usize) {
        match self.multi_edit.as_mut() {
            Some(multi_edit) => multi_edit.edits.push((range.clone(), new_len)),
            None => self.map_extra_selections_for_edit(range, new_len),
        }
    }

    /// Shift the extra selections past an edit made outside
    /// [`Self::edit_each_selection`] (undo, a completion, a search replace…).
    fn map_extra_selections_for_edit(&mut self, range: &Range<usize>, new_len: usize) {
        if self.extra_selections.is_empty() {
            return;
        }
        let edits = [(range.clone(), new_len)];
        for sel in self.extra_selections.iter_mut() {
            map_selection(sel, &edits);
        }
        let primary = ExtraSelection {
            range: self.selected_range,
            reversed: self.selection_reversed,
            preferred_column: self.preferred_column,
        };
        let mut all = std::mem::take(&mut self.extra_selections);
        all.push(primary);
        let ix = all.len() - 1;
        self.set_all_selections(all, ix);
    }

    /// Wrap an action handler written for the primary selection so it runs
    /// once per selection (see [`Self::edit_each_selection`]).
    ///
    /// While the completion / code-action menu is open the handler runs once,
    /// so the menu sees each keystroke a single time.
    pub fn each_selection<A: 'static>(
        handler: fn(&mut Self, &A, &mut Window, &mut Context<Self>),
    ) -> impl Fn(&mut Self, &A, &mut Window, &mut Context<Self>) + 'static {
        move |this, action, window, cx| {
            if this.extra_selections.is_empty() || this.is_context_menu_open(cx) {
                handler(this, action, window, cx);
                return;
            }
            // An inline completion belongs to one caret; Tab indents them all.
            this.clear_inline_completion(cx);
            this.edit_each_selection(window, cx, |this, window, cx| {
                handler(this, action, window, cx)
            });
            this.pause_blink_cursor(cx);
        }
    }

    /// Typing (and an IME commit) with several cursors: the text goes in at
    /// every one of them. Only the primary pass uses the platform's range,
    /// which points at the primary's marked text.
    pub(super) fn replace_text_at_each_selection(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui::EntityInputHandler as _;
        self.edit_each_selection(window, cx, |this, window, cx| {
            let is_primary = this.multi_edit.as_ref().is_some_and(|m| m.is_primary);
            let range = if is_primary {
                range_utf16.clone()
            } else {
                None
            };
            this.replace_text_in_range(range, new_text, window, cx);
        });
    }

    /// Copy with several selections: their texts, one per line.
    pub(super) fn copy_selections(&self, cx: &mut Context<Self>) -> bool {
        if self.extra_selections.is_empty() {
            return false;
        }
        let texts: Vec<String> = self
            .selected_ranges()
            .into_iter()
            .filter(|r| !r.is_empty())
            .map(|r| self.text.slice(r).to_string())
            .collect();
        if !texts.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(texts.join("\n")));
        }
        true
    }

    /// Cut with several selections.
    pub(super) fn cut_selections(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.copy_selections(cx) {
            return false;
        }
        self.edit_each_selection(window, cx, |this, window, cx| {
            if !this.selected_range.is_empty() {
                this.replace_text_in_range_silent(None, "", window, cx);
            }
        });
        true
    }

    /// Paste with several selections: one clipboard line per selection when
    /// the counts match, the whole clipboard at each otherwise.
    pub(super) fn paste_at_selections(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.extra_selections.is_empty() {
            return false;
        }
        let pieces = distribute_paste(text, self.selection_count());
        self.edit_each_selection_indexed(window, cx, |this, ix, window, cx| {
            let piece = pieces.as_ref().map_or(text, |p| p[ix].as_str());
            this.replace_text_in_range_silent(None, piece, window, cx);
        });
        let cursor = self.cursor();
        self.scroll_to(cursor, None, cx);
        true
    }

    /// ⌥-click: add a cursor at `offset`, or remove the one there. Returns
    /// `true` if a cursor was added.
    pub(super) fn toggle_cursor_at(&mut self, offset: usize, cx: &mut Context<Self>) -> bool {
        let (mut all, primary) = self.take_all_selections();
        let hit = all.iter().position(|s| {
            s.range.contains(offset) || (s.range.is_empty() && s.range.start == offset)
        });
        let added = hit.is_none();
        match hit {
            Some(ix) if all.len() > 1 => {
                all.remove(ix);
                // The newest remaining selection takes over as primary.
                let primary = if ix == primary {
                    all.len() - 1
                } else if ix < primary {
                    primary - 1
                } else {
                    primary
                };
                self.set_all_selections(all, primary);
            }
            Some(_) => {
                // The only cursor: keep it.
                self.set_all_selections(all, primary);
            }
            None => {
                all.push(ExtraSelection::new(offset..offset, false));
                let primary = all.len() - 1;
                self.set_all_selections(all, primary);
            }
        }
        self.update_preferred_column();
        self.pause_blink_cursor(cx);
        cx.notify();
        added
    }

    /// Mouse down with ⌥ held in the code editor. Returns `true` if handled.
    pub(super) fn handle_alt_click(
        &mut self,
        event: &MouseDownEvent,
        offset: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.mode.is_code_editor()
            || !self.mode.is_multi_line()
            || event.button != MouseButton::Left
            || !event.modifiers.alt
            || event.modifiers.shift
            || event.modifiers.secondary()
            || event.click_count != 1
        {
            return false;
        }
        // A drag from a new cursor extends it into a selection.
        self.selecting = self.toggle_cursor_at(offset, cx);
        true
    }

    /// Expand every empty selection to the word under it. Returns `true` if
    /// any selection was empty.
    fn expand_empty_selections_to_words(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let any_empty = self.selected_range.is_empty()
            || self.extra_selections.iter().any(|s| s.range.is_empty());
        if !any_empty {
            return false;
        }
        self.edit_each_selection(window, cx, |this, _, _| {
            if this.selected_range.is_empty()
                && let Some(word) = word_at(&this.text, this.cursor())
            {
                this.selected_range = word.into();
                this.selection_reversed = false;
            }
        });
        true
    }

    pub(super) fn select_next_occurrence(
        &mut self,
        _: &SelectNextOccurrence,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.expand_empty_selections_to_words(window, cx) {
            self.occurrence_whole_word = true;
            let cursor = self.cursor();
            self.scroll_to(cursor, None, cx);
            cx.notify();
            return;
        }
        let needle = self.text.slice(self.selected_range).to_string();
        let text = self.text.to_string();
        if self.extra_selections.is_empty() {
            // Whole words only if the selection came from a caret ⌘D and is
            // still that word.
            self.occurrence_whole_word =
                self.occurrence_whole_word && is_whole_word(&text, &self.selected_range.into());
        }
        let taken = self.selected_ranges();
        let Some(next) = find_next_occurrence(
            &text,
            &needle,
            self.selected_range.end,
            self.occurrence_whole_word,
            &taken,
        ) else {
            return;
        };

        let (mut all, _) = self.take_all_selections();
        all.push(ExtraSelection::new(next.clone(), false));
        let primary = all.len() - 1;
        self.set_all_selections(all, primary);
        self.update_preferred_column();
        self.scroll_to(next.end, None, cx);
        self.pause_blink_cursor(cx);
        cx.notify();
    }

    pub(super) fn select_all_occurrences(
        &mut self,
        _: &SelectAllOccurrences,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let whole_word = if self.selected_range.is_empty() {
            let Some(word) = word_at(&self.text, self.cursor()) else {
                return;
            };
            self.selected_range = word.into();
            self.selection_reversed = false;
            true
        } else {
            self.occurrence_whole_word
                && is_whole_word(&self.text.to_string(), &self.selected_range.into())
        };
        self.extra_selections.clear();

        let needle = self.text.slice(self.selected_range).to_string();
        let text = self.text.to_string();
        let primary_range: Range<usize> = self.selected_range.into();
        let mut all: Vec<ExtraSelection> = find_occurrences(&text, &needle, whole_word)
            .into_iter()
            .map(|r| ExtraSelection::new(r, false))
            .collect();
        let primary = all
            .iter()
            .position(|s| Range::from(s.range) == primary_range)
            .unwrap_or_else(|| {
                all.push(ExtraSelection::new(primary_range.clone(), false));
                all.len() - 1
            });
        self.occurrence_whole_word = whole_word;
        self.set_all_selections(all, primary);
        self.update_preferred_column();
        self.pause_blink_cursor(cx);
        cx.notify();
    }

    pub(super) fn add_cursor_above(
        &mut self,
        _: &AddCursorAbove,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_cursor_vertically(MoveDirection::Up, window, cx);
    }

    pub(super) fn add_cursor_below(
        &mut self,
        _: &AddCursorBelow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_cursor_vertically(MoveDirection::Down, window, cx);
    }

    /// Add a cursor one row above the topmost / below the bottommost caret,
    /// keeping that caret's column.
    fn add_cursor_vertically(
        &mut self,
        direction: MoveDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.is_multi_line() || self.last_layout.is_none() {
            return;
        }
        let (all, primary) = self.take_all_selections();
        let edge = match direction {
            MoveDirection::Up => all.iter().enumerate().min_by_key(|(_, s)| s.head()),
            MoveDirection::Down => all.iter().enumerate().max_by_key(|(_, s)| s.head()),
        }
        .map(|(ix, s)| (ix, *s));
        let Some((edge_ix, edge)) = edge else {
            self.set_all_selections(all, primary);
            return;
        };

        // Borrow the primary slot to run the ordinary vertical move from the
        // edge caret.
        let head = edge.head();
        self.selected_range = (head..head).into();
        self.selection_reversed = false;
        self.preferred_column = edge.preferred_column;
        if self.preferred_column.is_none() {
            self.update_preferred_column();
        }
        let preferred_column = self.preferred_column;
        let lines = match direction {
            MoveDirection::Up => -1,
            MoveDirection::Down => 1,
        };
        self.move_vertical(lines, window, cx);
        let new_head = self.cursor();

        let mut all = all;
        if new_head == head {
            // Already on the first / last row.
            self.set_all_selections(all, primary);
        } else {
            if edge_ix < all.len() && all[edge_ix].preferred_column.is_none() {
                all[edge_ix].preferred_column = preferred_column;
            }
            all.push(ExtraSelection {
                range: (new_head..new_head).into(),
                reversed: false,
                preferred_column,
            });
            let primary = all.len() - 1;
            self.set_all_selections(all, primary);
        }
        self.pause_blink_cursor(cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Root, input::Undo, theme::Theme};
    use gpui::{
        AppContext as _, Entity, EntityInputHandler as _, TestAppContext, VisualTestContext,
    };

    fn sel(range: Range<usize>) -> ExtraSelection {
        ExtraSelection::new(range, false)
    }

    fn ranges(selections: &[ExtraSelection]) -> Vec<Range<usize>> {
        selections.iter().map(|s| s.range.into()).collect()
    }

    #[test]
    fn test_map_offset() {
        // Insert 3 bytes at 5.
        assert_eq!(map_offset(4, &(5..5), 3), 4);
        assert_eq!(map_offset(5, &(5..5), 3), 5);
        assert_eq!(map_offset(6, &(5..5), 3), 9);
        // Replace 5..8 with 1 byte.
        assert_eq!(map_offset(8, &(5..8), 1), 6);
        assert_eq!(map_offset(7, &(5..8), 1), 6);
        assert_eq!(map_offset(6, &(5..8), 1), 6);
        // Delete 5..8.
        assert_eq!(map_offset(7, &(5..8), 0), 5);
        assert_eq!(map_offset(10, &(5..8), 0), 7);
    }

    #[test]
    fn test_merge_selections() {
        // Overlapping selections merge, and the merged one stays primary.
        let (merged, primary) = merge_selections(vec![sel(4..8), sel(0..2), sel(6..10)], 2);
        assert_eq!(ranges(&merged), vec![0..2, 4..10]);
        assert_eq!(primary, 1);

        // Two carets at one spot become one.
        let (merged, primary) = merge_selections(vec![sel(3..3), sel(3..3), sel(7..7)], 2);
        assert_eq!(ranges(&merged), vec![3..3, 7..7]);
        assert_eq!(primary, 1);

        // Touching selections stay apart — "aa" selected as two occurrences
        // of "a" — but a caret touching a selection joins it.
        let (merged, _) = merge_selections(vec![sel(0..1), sel(1..2)], 0);
        assert_eq!(ranges(&merged), vec![0..1, 1..2]);
        let (merged, _) = merge_selections(vec![sel(0..3), sel(3..3)], 0);
        assert_eq!(ranges(&merged), vec![0..3]);
    }

    #[test]
    fn test_find_occurrences() {
        let text = "foo foobar foo_bar foo";
        assert_eq!(
            find_occurrences(text, "foo", false),
            vec![0..3, 4..7, 11..14, 19..22]
        );
        assert_eq!(find_occurrences(text, "foo", true), vec![0..3, 19..22]);
        assert!(find_occurrences(text, "", false).is_empty());
        // Non-overlapping.
        assert_eq!(find_occurrences("aaaa", "aa", false), vec![0..2, 2..4]);
        // Multi-byte text.
        assert_eq!(
            find_occurrences("中文 中文", "中文", true),
            vec![0..6, 7..13]
        );
    }

    #[test]
    fn test_find_next_occurrence_wraps_and_skips_taken() {
        let text = "a x a x a";
        assert_eq!(
            find_next_occurrence(text, "a", 1, true, &[0..1]),
            Some(4..5)
        );
        // Wraps around past the end.
        assert_eq!(
            find_next_occurrence(text, "a", 9, true, &[8..9, 4..5]),
            Some(0..1)
        );
        // All taken.
        assert_eq!(
            find_next_occurrence(text, "a", 0, true, &[0..1, 4..5, 8..9]),
            None
        );
    }

    #[test]
    fn test_distribute_paste() {
        assert_eq!(
            distribute_paste("one\ntwo\nthree", 3),
            Some(vec!["one".into(), "two".into(), "three".into()])
        );
        // A trailing newline and CRLF are tolerated.
        assert_eq!(
            distribute_paste("one\r\ntwo\r\n", 2),
            Some(vec!["one".into(), "two".into()])
        );
        assert_eq!(distribute_paste("one\ntwo", 3), None);
        assert_eq!(distribute_paste("one", 1), None);
    }

    fn build(cx: &mut TestAppContext, text: &str) -> (Entity<InputState>, VisualTestContext) {
        let mut input: Option<Entity<InputState>> = None;
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                cx.set_global(Theme::default());
                crate::input::init(cx);
                input = Some(cx.new(|cx| InputState::new(window, cx).code_editor("rust")));
                cx.new(|cx| Root::new(input.clone().unwrap(), window, cx))
            })
            .unwrap()
        });
        let input = input.unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let text = text.to_string();
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value(text, window, cx);
                // No time-based grouping: every edit is its own undo step
                // unless something groups it on purpose.
                state.history = crate::history::History::new();
            });
        });
        cx.run_until_parked();
        (input, cx)
    }

    fn value(input: &Entity<InputState>, cx: &mut VisualTestContext) -> String {
        cx.update(|_, cx| input.read(cx).value().to_string())
    }

    #[gpui::test]
    fn test_select_next_occurrence(cx: &mut TestAppContext) {
        let (input, mut cx) = build(cx, "foo bar foo foobar foo");
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_ranges([1..1], cx);
                // First ⌘D: the word under the caret.
                state.select_next_occurrence(&SelectNextOccurrence, window, cx);
                assert_eq!(state.selected_ranges(), vec![0..3]);
                // Then whole-word matches only: `foobar` is skipped.
                state.select_next_occurrence(&SelectNextOccurrence, window, cx);
                assert_eq!(state.selected_ranges(), vec![0..3, 8..11]);
                assert_eq!(state.selected_range(), 8..11);
                state.select_next_occurrence(&SelectNextOccurrence, window, cx);
                assert_eq!(state.selected_ranges(), vec![0..3, 8..11, 19..22]);
                // Nothing left: no change.
                state.select_next_occurrence(&SelectNextOccurrence, window, cx);
                assert_eq!(state.selection_count(), 3);
            });
        });
    }

    #[gpui::test]
    fn test_select_next_occurrence_wraps_around(cx: &mut TestAppContext) {
        let (input, mut cx) = build(cx, "ab ab ab");
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_ranges([3..5], cx);
                state.select_next_occurrence(&SelectNextOccurrence, window, cx);
                state.select_next_occurrence(&SelectNextOccurrence, window, cx);
                assert_eq!(state.selected_ranges(), vec![0..2, 3..5, 6..8]);
                assert_eq!(state.selected_range(), 0..2);
            });
        });
    }

    #[gpui::test]
    fn test_select_all_occurrences_and_type(cx: &mut TestAppContext) {
        let (input, mut cx) = build(cx, "let a = 1;\nlet b = a + a;\n");
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_ranges([4..4], cx);
                state.select_all_occurrences(&SelectAllOccurrences, window, cx);
                assert_eq!(state.selected_ranges(), vec![4..5, 19..20, 23..24]);
                assert_eq!(state.selected_range(), 4..5);

                // Typing replaces every selection, as one undo step.
                state.replace_text_in_range(None, "xy", window, cx);
                assert_eq!(state.value(), "let xy = 1;\nlet b = xy + xy;\n");
                assert_eq!(state.selected_ranges(), vec![6..6, 22..22, 27..27]);

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "let a = 1;\nlet b = a + a;\n");
            });
        });
    }

    #[gpui::test]
    fn test_multi_cursor_edits_are_one_undo_step(cx: &mut TestAppContext) {
        let (input, mut cx) = build(cx, "one\ntwo\nthree");
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                // An earlier, separate edit.
                state.set_selected_ranges([0..0], cx);
                state.replace_text_in_range(None, "#", window, cx);
                assert_eq!(state.value(), "#one\ntwo\nthree");

                state.set_selected_ranges([4..4, 8..8, 14..14], cx);
                state.replace_text_in_range(None, "!", window, cx);
                assert_eq!(state.value(), "#one!\ntwo!\nthree!");

                let backspace = InputState::each_selection(InputState::backspace);
                backspace(state, &crate::input::Backspace, window, cx);
                backspace(state, &crate::input::Backspace, window, cx);
                assert_eq!(state.value(), "#on\ntw\nthre");

                // Undo the two backspaces (one step each), then the typing.
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "#one\ntwo\nthree");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "#one!\ntwo!\nthree!");
            });
        });
        // `undo` collapses to one cursor.
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "#one\ntwo\nthree");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "one\ntwo\nthree");
            });
        });
    }

    #[gpui::test]
    fn test_paste_distributes_lines(cx: &mut TestAppContext) {
        let (input, mut cx) = build(cx, "a\nb\nc");
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_ranges([1..1, 3..3, 5..5], cx);
                assert!(state.paste_at_selections("1\n2\n3", window, cx));
                assert_eq!(state.value(), "a1\nb2\nc3");

                // A mismatched count pastes everything at every cursor.
                assert!(state.paste_at_selections("xy", window, cx));
                assert_eq!(state.value(), "a1xy\nb2xy\nc3xy");
            });
        });
    }

    #[gpui::test]
    fn test_copy_joins_selections(cx: &mut TestAppContext) {
        let (input, mut cx) = build(cx, "alpha beta gamma");
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_ranges([11..16, 0..5], cx);
                assert!(state.cut_selections(window, cx));
                assert_eq!(state.value(), " beta ");
            });
            assert_eq!(
                cx.read_from_clipboard().and_then(|c| c.text()),
                Some("alpha\ngamma".to_string())
            );
        });
    }

    #[gpui::test]
    fn test_indent_and_enter_at_each_cursor(cx: &mut TestAppContext) {
        let (input, mut cx) = build(cx, "a\nb");
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_ranges([0..0, 2..2], cx);
                let indent = InputState::each_selection(InputState::indent_inline);
                indent(state, &crate::input::IndentInline, window, cx);
                assert_eq!(state.value(), "  a\n  b");
                assert_eq!(state.selected_ranges(), vec![2..2, 6..6]);

                let right = InputState::each_selection(InputState::right);
                right(state, &crate::input::MoveRight, window, cx);
                state.enter(
                    &crate::input::Enter {
                        secondary: false,
                        shift: false,
                    },
                    window,
                    cx,
                );
                // Each new line keeps its line's indent.
                assert_eq!(state.value(), "  a\n  \n  b\n  ");
                assert_eq!(state.selection_count(), 2);
            });
        });
    }

    #[gpui::test]
    fn test_add_cursor_below_and_escape(cx: &mut TestAppContext) {
        let (input, mut cx) = build(cx, "abc\nabc\nabc");
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_ranges([1..1], cx);
                state.add_cursor_below(&AddCursorBelow, window, cx);
                state.add_cursor_below(&AddCursorBelow, window, cx);
                assert_eq!(state.selected_ranges(), vec![1..1, 5..5, 9..9]);
                // The last row: nothing to add.
                state.add_cursor_below(&AddCursorBelow, window, cx);
                assert_eq!(state.selection_count(), 3);

                state.replace_text_in_range(None, "-", window, cx);
                assert_eq!(state.value(), "a-bc\na-bc\na-bc");

                state.escape(&crate::input::Escape, window, cx);
                assert_eq!(state.selection_count(), 1);
                assert_eq!(state.selected_range(), 12..12);
            });
        });
    }

    #[gpui::test]
    fn test_moves_merge_cursors(cx: &mut TestAppContext) {
        let (input, mut cx) = build(cx, "ab\ncd");
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_ranges([0..0, 1..1], cx);
                let left = InputState::each_selection(InputState::left);
                left(state, &crate::input::MoveLeft, window, cx);
                // Both carets reach the start and merge.
                assert_eq!(state.selected_ranges(), vec![0..0]);

                state.set_selected_ranges([0..0, 3..3], cx);
                let select_right = InputState::each_selection(InputState::select_right);
                select_right(state, &crate::actions::SelectRight, window, cx);
                assert_eq!(state.selected_ranges(), vec![0..1, 3..4]);
            });
        });
    }

    #[gpui::test]
    fn test_vertical_moves_keep_each_column(cx: &mut TestAppContext) {
        let (input, mut cx) = build(cx, "abc\nabc\nabc");
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_ranges([6..6, 2..2], cx);
                let down = InputState::each_selection(InputState::down);
                down(state, &crate::input::MoveDown, window, cx);
                assert_eq!(state.selected_ranges(), vec![6..6, 10..10]);
                let up = InputState::each_selection(InputState::up);
                up(state, &crate::input::MoveUp, window, cx);
                assert_eq!(state.selected_ranges(), vec![2..2, 6..6]);
                // The top caret can't go higher; the other one meets it.
                up(state, &crate::input::MoveUp, window, cx);
                assert_eq!(state.selected_ranges(), vec![2..2]);
            });
        });
    }

    #[gpui::test]
    fn test_toggle_cursor(cx: &mut TestAppContext) {
        let (input, mut cx) = build(cx, "hello world");
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_ranges([0..0], cx);
                state.toggle_cursor_at(6, cx);
                assert_eq!(state.selected_ranges(), vec![0..0, 6..6]);
                assert_eq!(state.selected_range(), 6..6);
                // Clicking an existing cursor removes it.
                state.toggle_cursor_at(0, cx);
                assert_eq!(state.selected_ranges(), vec![6..6]);
                // …but never the last one.
                state.toggle_cursor_at(6, cx);
                assert_eq!(state.selected_ranges(), vec![6..6]);
            });
        });
    }

    #[gpui::test]
    fn test_outside_edit_shifts_extra_cursors(cx: &mut TestAppContext) {
        let (input, mut cx) = build(cx, "ab\ncd");
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_ranges([4..4, 1..1], cx);
                // An edit made directly (like a completion) at the primary.
                state.replace_text_in_range_silent(None, "XYZ", window, cx);
                assert_eq!(state.value(), "aXYZb\ncd");
                assert_eq!(state.selected_ranges(), vec![4..4, 7..7]);
            });
        });
        assert_eq!(value(&input, &mut cx), "aXYZb\ncd");
    }
}
