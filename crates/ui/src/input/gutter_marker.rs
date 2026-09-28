//! Per-line decorations painted at the edge of the code editor's gutter —
//! the colored bars an editor draws beside lines a VCS sees as added or
//! modified, and the small wedge between two lines where some were deleted.
//!
//! The editor knows nothing about where the markers come from. The host
//! computes them (typically from a diff against a VCS base) and hands them
//! over with [`InputState::set_gutter_markers`]; between two such updates the
//! markers are shifted along with each edit so they stay on the lines they
//! were describing (see [`adjust_markers_for_edit`]).
use std::{ops::Range, rc::Rc};

use gpui::{App, Context, Hsla, Pixels, Point, Window};

use crate::ActiveTheme as _;
use crate::highlighter::DiagnosticSeverity;
use ropey::Rope;

use super::{InputState, RopeExt as _};

/// What a [`GutterMarker`] says about its lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GutterMarkerKind {
    /// Lines that are new. Painted as a bar.
    Added,
    /// Lines that replaced others. Painted as a bar.
    Modified,
    /// Lines that were removed at a boundary. Painted as a wedge on the
    /// boundary *above* `lines.start`; `lines` is empty.
    Deleted,
}

/// A decoration covering a range of buffer lines (0-based, end exclusive).
#[derive(Clone, Debug, PartialEq)]
pub struct GutterMarker {
    /// The buffer lines the marker covers. For [`GutterMarkerKind::Deleted`]
    /// this is the empty range `n..n`, meaning the boundary between line
    /// `n - 1` and line `n`; `n` equal to the line count means "after the last
    /// line".
    pub lines: Range<usize>,
    pub kind: GutterMarkerKind,
    /// Overrides the kind's default color (success / warning / danger).
    pub color: Option<Hsla>,
}

impl GutterMarker {
    pub fn new(lines: Range<usize>, kind: GutterMarkerKind) -> Self {
        let lines = match kind {
            GutterMarkerKind::Deleted => lines.start..lines.start,
            _ => lines.start..lines.end.max(lines.start + 1),
        };
        Self {
            lines,
            kind,
            color: None,
        }
    }

    pub fn added(lines: Range<usize>) -> Self {
        Self::new(lines, GutterMarkerKind::Added)
    }

    pub fn modified(lines: Range<usize>) -> Self {
        Self::new(lines, GutterMarkerKind::Modified)
    }

    /// Lines were deleted just above buffer line `line`.
    pub fn deleted(line: usize) -> Self {
        Self::new(line..line, GutterMarkerKind::Deleted)
    }

    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }

    /// The last line (exclusive) this marker has anything to say about: a
    /// deleted marker sits on a boundary, but belongs to the line below it.
    fn end_for_search(&self) -> usize {
        self.lines.end.max(self.lines.start + 1)
    }
}

/// A click on a painted gutter marker.
#[derive(Clone, Debug)]
pub struct GutterMarkerClick {
    /// The marker that was clicked. For a folded region summarizing several
    /// markers, the first of them.
    pub marker: GutterMarker,
    /// The buffer line the click landed on (the fold's first line for a
    /// folded region).
    pub line: usize,
    /// Window coordinates of the click, to anchor a popover to.
    pub position: Point<Pixels>,
}

pub(super) type GutterMarkerClickHandler = Rc<dyn Fn(&GutterMarkerClick, &mut Window, &mut App)>;

#[derive(Default)]
pub(super) struct GutterMarkers {
    /// Sorted by `lines.start`.
    pub(super) markers: Vec<GutterMarker>,
    pub(super) on_click: Option<GutterMarkerClickHandler>,
}

impl InputState {
    /// Replace the gutter markers. They need not be sorted.
    pub fn set_gutter_markers(&mut self, mut markers: Vec<GutterMarker>, cx: &mut Context<Self>) {
        markers.sort_by_key(|m| (m.lines.start, m.kind != GutterMarkerKind::Deleted));
        if self.gutter_markers.markers != markers {
            self.gutter_markers.markers = markers;
            cx.notify();
        }
    }

    /// The current gutter markers, sorted by first line, shifted by any edits
    /// made since they were set.
    pub fn gutter_markers(&self) -> &[GutterMarker] {
        &self.gutter_markers.markers
    }

    /// Called when a painted gutter marker is clicked. The click does not move
    /// the cursor.
    pub fn on_gutter_marker_click(
        &mut self,
        handler: impl Fn(&GutterMarkerClick, &mut Window, &mut App) + 'static,
    ) {
        self.gutter_markers.on_click = Some(Rc::new(handler));
    }

    /// Shift the markers for an edit. Call with the text *before* the edit.
    pub(super) fn adjust_gutter_markers_for_edit(
        &mut self,
        old_text: &Rope,
        range: &Range<usize>,
        new_text: &str,
    ) {
        if self.gutter_markers.markers.is_empty() {
            return;
        }
        let start_line = old_text.offset_to_point(range.start).row;
        let end_line = old_text.offset_to_point(range.end.min(old_text.len())).row;
        let new_lines = new_text.bytes().filter(|b| *b == b'\n').count();
        adjust_markers_for_edit(
            &mut self.gutter_markers.markers,
            start_line,
            end_line,
            new_lines,
        );
    }
}

/// Shift `markers` for an edit that replaced buffer lines
/// `start_line..=end_line` (partially, at least) with text spanning
/// `new_lines + 1` lines.
///
/// Markers wholly below the edit move with it, markers above stay, and a
/// marker the edit reaches into grows or shrinks with the line count. This is
/// only meant to keep the picture plausible until the host recomputes.
pub fn adjust_markers_for_edit(
    markers: &mut Vec<GutterMarker>,
    start_line: usize,
    end_line: usize,
    new_lines: usize,
) {
    let delta = new_lines as isize - (end_line - start_line) as isize;
    let shift = |n: usize| (n as isize + delta).max(0) as usize;
    // The last line the edit's own text ends on, after the edit.
    let new_end_line = start_line + new_lines;
    for m in markers.iter_mut() {
        if m.kind == GutterMarkerKind::Deleted {
            let s = m.lines.start;
            let s = if s > end_line {
                shift(s)
            } else if s <= start_line {
                s
            } else {
                s.min(new_end_line)
            };
            m.lines = s..s;
            continue;
        }
        let Range { start, end } = m.lines.clone();
        if start > end_line {
            m.lines = shift(start)..shift(end);
        } else if end <= start_line {
            // Above the edit.
        } else {
            let start = start.min(new_end_line);
            let end = if end > end_line {
                shift(end)
            } else {
                end.min(new_end_line + 1)
            };
            m.lines = start..end.max(start + 1);
        }
    }
}

/// Where on a visible row a marker is painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MarkPlacement {
    /// A bar along the whole row (every wrapped sub-row of it).
    Bar,
    /// A wedge on the row's top edge.
    WedgeTop,
    /// A wedge on the row's bottom edge (a deletion after the last line).
    WedgeBottom,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct VisibleMark {
    /// Index into the visible buffer lines.
    pub(super) row: usize,
    pub(super) placement: MarkPlacement,
    pub(super) kind: GutterMarkerKind,
    pub(super) color: Option<Hsla>,
    /// Index of the (first) marker this mark stands for.
    pub(super) marker: usize,
}

/// Map sorted `markers` onto the `visible` buffer lines (ascending).
///
/// `hidden_after(line)` is the end (exclusive) of the lines a fold starting at
/// `line` hides — `line + 1` when nothing is folded there. Markers on hidden
/// lines are summarized on the fold's first line: one kind if they all agree,
/// otherwise Modified.
pub(super) fn visible_marks(
    markers: &[GutterMarker],
    visible: &[usize],
    total_lines: usize,
    hidden_after: impl Fn(usize) -> usize,
) -> Vec<VisibleMark> {
    let mut out = Vec::new();
    if markers.is_empty() {
        return out;
    }
    let last_line = total_lines.saturating_sub(1);
    for (row, &line) in visible.iter().enumerate() {
        let cover_end = hidden_after(line).max(line + 1);
        let first = markers.partition_point(|m| m.end_for_search() <= line);
        let mut bar: Option<(GutterMarkerKind, Option<Hsla>, usize)> = None;
        let mut mixed = false;
        for (ix, m) in markers.iter().enumerate().skip(first) {
            if m.lines.start >= cover_end {
                break;
            }
            if m.kind == GutterMarkerKind::Deleted {
                if m.lines.start == line {
                    out.push(VisibleMark {
                        row,
                        placement: MarkPlacement::WedgeTop,
                        kind: m.kind,
                        color: m.color,
                        marker: ix,
                    });
                    continue;
                }
                if m.lines.start < line {
                    continue;
                }
                // A deletion inside the fold: counts toward the summary.
            }
            match bar {
                None => bar = Some((m.kind, m.color, ix)),
                Some((kind, _, _)) if kind != m.kind => mixed = true,
                Some(_) => {}
            }
        }
        if let Some((kind, color, marker)) = bar {
            let (kind, color) = if mixed {
                (GutterMarkerKind::Modified, None)
            } else {
                (kind, color)
            };
            out.push(VisibleMark {
                row,
                placement: MarkPlacement::Bar,
                kind,
                color,
                marker,
            });
        }
        // A deletion after the last line has no row of its own.
        if line == last_line && cover_end >= total_lines {
            for (ix, m) in markers.iter().enumerate().skip(first) {
                if m.kind == GutterMarkerKind::Deleted && m.lines.start >= total_lines {
                    out.push(VisibleMark {
                        row,
                        placement: MarkPlacement::WedgeBottom,
                        kind: m.kind,
                        color: m.color,
                        marker: ix,
                    });
                    break;
                }
            }
        }
    }
    out
}

/// How loudly a diagnostic speaks for its line: only errors and warnings
/// earn the line number a color or the overview ruler a tick.
fn problem_rank(severity: DiagnosticSeverity) -> u8 {
    match severity {
        DiagnosticSeverity::Error => 2,
        DiagnosticSeverity::Warning => 1,
        _ => 0,
    }
}

/// The worst problem on each line that has an error or a warning among the
/// diagnostics starting in `bytes`, as `(line, rank, severity)` sorted by line.
fn problem_lines(
    set: &crate::highlighter::DiagnosticSet,
    bytes: Option<Range<usize>>,
) -> Vec<(usize, DiagnosticSeverity)> {
    let mut worst: std::collections::BTreeMap<usize, DiagnosticSeverity> = Default::default();
    let mut note = |entry: &crate::highlighter::DiagnosticEntry| {
        let severity = entry.diagnostic.severity;
        if problem_rank(severity) == 0 {
            return;
        }
        let line = entry.diagnostic.range.start.line as usize;
        let slot = worst.entry(line).or_insert(severity);
        if problem_rank(severity) > problem_rank(*slot) {
            *slot = severity;
        }
    };
    match bytes {
        Some(bytes) => set.range(bytes).for_each(&mut note),
        None => set.iter().take(MAX_RULER_DIAGNOSTICS).for_each(&mut note),
    }
    worst.into_iter().collect()
}

/// Past this many diagnostics the ruler stops reading them: a file that bad
/// is solid color anyway.
const MAX_RULER_DIAGNOSTICS: usize = 5000;

impl InputState {
    /// The color the line number of each visible line with an error or a
    /// warning is drawn in.
    pub(super) fn problem_line_number_colors(
        &self,
        visible_bytes: &Range<usize>,
        cx: &App,
    ) -> std::collections::HashMap<usize, Hsla> {
        let Some(set) = self.diagnostics().filter(|set| !set.is_empty()) else {
            return Default::default();
        };
        problem_lines(set, Some(visible_bytes.clone()))
            .into_iter()
            .map(|(line, severity)| (line, severity.fg(cx)))
            .collect()
    }

    /// Paint the overview ruler: a tick on the scrollbar track for every
    /// gutter marker (left column) and every line with an error or a warning
    /// (right column), at the height the scrollbar thumb would sit to show it.
    pub(super) fn paint_overview_ruler(
        &self,
        track: gpui::Bounds<Pixels>,
        scroll_height: Pixels,
        window: &mut Window,
        cx: &App,
    ) {
        use super::display_map::BufferPoint;
        use gpui::{fill, point, px, size};

        let Some(line_height) = self.last_layout.as_ref().map(|l| l.line_height) else {
            return;
        };
        // Nothing to scroll to: everything the ruler would point at is on
        // screen already.
        if scroll_height <= track.size.height || line_height <= px(0.) {
            return;
        }
        let diagnostics = self
            .diagnostics()
            .filter(|set| !set.is_empty())
            .map(|set| problem_lines(set, None))
            .unwrap_or_default();
        if self.gutter_markers.markers.is_empty() && diagnostics.is_empty() {
            return;
        }
        let scale = track.size.height / scroll_height;
        let row_of = |line: usize| {
            self.display_map
                .buffer_pos_to_display_pos(BufferPoint::new(line, 0))
                .row
        };
        let tick = |rows: Range<usize>, column: usize, color: Hsla, window: &mut Window| {
            const TICK_WIDTH: Pixels = px(3.);
            let top = track.origin.y + line_height * rows.start as f32 * scale;
            let height = (line_height * rows.len().max(1) as f32 * scale).max(px(2.));
            let x = track.right() - px(if column == 0 { 9. } else { 5. });
            window.paint_quad(fill(
                gpui::Bounds::new(point(x, top), size(TICK_WIDTH, height)),
                color,
            ));
        };
        let theme = cx.theme();
        for marker in &self.gutter_markers.markers {
            let color = marker.color.unwrap_or(match marker.kind {
                GutterMarkerKind::Added => theme.success,
                GutterMarkerKind::Modified => theme.warning,
                GutterMarkerKind::Deleted => theme.danger,
            });
            let start = row_of(marker.lines.start);
            let end = if marker.lines.is_empty() {
                start
            } else {
                row_of(marker.lines.end - 1) + 1
            };
            tick(start..end, 0, color.opacity(0.8), window);
        }
        for (line, severity) in diagnostics {
            let row = row_of(line);
            tick(row..row + 1, 1, severity.fg(cx), window);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use GutterMarkerKind::*;

    fn lines(ms: &[GutterMarker]) -> Vec<(Range<usize>, GutterMarkerKind)> {
        ms.iter().map(|m| (m.lines.clone(), m.kind)).collect()
    }

    #[test]
    fn markers_below_an_edit_shift_with_it() {
        let mut ms = vec![
            GutterMarker::added(1..3),
            GutterMarker::modified(10..12),
            GutterMarker::deleted(20),
        ];
        // Two new lines typed on line 5.
        adjust_markers_for_edit(&mut ms, 5, 5, 2);
        assert_eq!(
            lines(&ms),
            vec![(1..3, Added), (12..14, Modified), (22..22, Deleted)]
        );
        // Lines 5..=7 joined into one.
        adjust_markers_for_edit(&mut ms, 5, 7, 0);
        assert_eq!(
            lines(&ms),
            vec![(1..3, Added), (10..12, Modified), (20..20, Deleted)]
        );
    }

    #[test]
    fn a_marker_the_edit_reaches_into_grows_and_shrinks() {
        let mut ms = vec![GutterMarker::modified(4..8)];
        adjust_markers_for_edit(&mut ms, 5, 5, 1);
        assert_eq!(lines(&ms), vec![(4..9, Modified)]);
        // Deleting lines 3..=9 entirely leaves a one-line marker where it was.
        adjust_markers_for_edit(&mut ms, 3, 9, 0);
        assert_eq!(lines(&ms), vec![(3..4, Modified)]);
    }

    #[test]
    fn a_deletion_boundary_stays_put_for_an_edit_on_its_own_line() {
        let mut ms = vec![GutterMarker::deleted(4)];
        adjust_markers_for_edit(&mut ms, 4, 4, 0);
        assert_eq!(lines(&ms), vec![(4..4, Deleted)]);
        // Enter at the end of the line above pushes it down.
        adjust_markers_for_edit(&mut ms, 3, 3, 1);
        assert_eq!(lines(&ms), vec![(5..5, Deleted)]);
    }

    fn no_folds(line: usize) -> usize {
        line + 1
    }

    #[test]
    fn markers_map_onto_visible_rows() {
        let ms = vec![
            GutterMarker::added(1..3),
            GutterMarker::deleted(5),
            GutterMarker::modified(6..7),
        ];
        let visible: Vec<usize> = (0..8).collect();
        let marks = visible_marks(&ms, &visible, 8, no_folds);
        let got: Vec<_> = marks.iter().map(|m| (m.row, m.placement, m.kind)).collect();
        assert_eq!(
            got,
            vec![
                (1, MarkPlacement::Bar, Added),
                (2, MarkPlacement::Bar, Added),
                (5, MarkPlacement::WedgeTop, Deleted),
                (6, MarkPlacement::Bar, Modified),
            ]
        );
    }

    #[test]
    fn a_scrolled_viewport_only_sees_its_own_lines() {
        let ms = vec![GutterMarker::added(0..50), GutterMarker::modified(90..91)];
        let visible: Vec<usize> = (40..60).collect();
        let marks = visible_marks(&ms, &visible, 100, no_folds);
        assert_eq!(marks.len(), 10);
        assert!(marks.iter().all(|m| m.kind == Added && m.marker == 0));
        assert_eq!(marks.last().unwrap().row, 9);
    }

    #[test]
    fn markers_inside_a_fold_are_summarized_on_its_first_line() {
        // Lines 2..=9 folded: 3..=8 hidden, 2 and 9 visible.
        let fold = |line: usize| if line == 2 { 9 } else { line + 1 };
        let visible = [0, 1, 2, 9, 10];
        let same = vec![GutterMarker::added(4..5), GutterMarker::added(7..8)];
        let marks = visible_marks(&same, &visible, 11, fold);
        assert_eq!(marks.len(), 1);
        assert_eq!((marks[0].row, marks[0].kind), (2, Added));

        let mixed = vec![GutterMarker::added(4..5), GutterMarker::deleted(7)];
        let marks = visible_marks(&mixed, &visible, 11, fold);
        assert_eq!(marks.len(), 1);
        assert_eq!((marks[0].row, marks[0].kind), (2, Modified));

        // A deletion right at the fold's visible last line is its own wedge.
        let edge = vec![GutterMarker::deleted(9)];
        let marks = visible_marks(&edge, &visible, 11, fold);
        assert_eq!(
            (marks[0].row, marks[0].placement),
            (3, MarkPlacement::WedgeTop)
        );
    }

    #[test]
    fn a_deletion_after_the_last_line_sits_under_it() {
        let ms = vec![GutterMarker::deleted(3)];
        let marks = visible_marks(&ms, &[0, 1, 2], 3, no_folds);
        assert_eq!(marks.len(), 1);
        assert_eq!(
            (marks[0].row, marks[0].placement),
            (2, MarkPlacement::WedgeBottom)
        );
    }

    #[gpui::test]
    fn typing_shifts_the_markers_until_the_host_recomputes(cx: &mut gpui::TestAppContext) {
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
                state.set_gutter_markers(
                    vec![GutterMarker::deleted(3), GutterMarker::modified(2..3)],
                    cx,
                );
                assert_eq!(
                    lines(state.gutter_markers()),
                    vec![(2..3, Modified), (3..3, Deleted)],
                    "sorted on the way in"
                );
                state.replace_text_in_range(Some(0..0), "x\n", window, cx);
                assert_eq!(
                    lines(state.gutter_markers()),
                    vec![(3..4, Modified), (4..4, Deleted)]
                );
            });
        });
    }

    #[test]
    fn only_errors_and_warnings_mark_a_line_and_the_worst_wins() {
        use crate::highlighter::{Diagnostic, DiagnosticSet};
        use lsp_types::Position;

        let text = Rope::from("a\nb\nc\nd\n");
        let mut set = DiagnosticSet::new(&text);
        let at = |line: u32| Position::new(line, 0)..Position::new(line, 1);
        set.push(Diagnostic::new(at(0), "hint").with_severity(DiagnosticSeverity::Hint));
        set.push(Diagnostic::new(at(1), "warn").with_severity(DiagnosticSeverity::Warning));
        set.push(Diagnostic::new(at(1), "err").with_severity(DiagnosticSeverity::Error));
        set.push(Diagnostic::new(at(3), "warn").with_severity(DiagnosticSeverity::Warning));
        assert_eq!(
            problem_lines(&set, None),
            vec![
                (1, DiagnosticSeverity::Error),
                (3, DiagnosticSeverity::Warning)
            ]
        );
        // Only what starts in the byte range asked about.
        assert_eq!(
            problem_lines(&set, Some(0..4)),
            vec![(1, DiagnosticSeverity::Error)]
        );
    }
}
