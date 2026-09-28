//! IDE-style editing commands for the code editor.
//!
//! Every command is split in two halves:
//!
//! - A pure text transform (`toggle_line_comment`, `move_lines`, `auto_pair`,
//!   ...) that takes the text plus a selection and returns an [`EditPlan`]:
//!   one contiguous replacement and the selection to put down afterwards.
//!   These know nothing about gpui, so they are unit tested directly, and a
//!   multi-cursor caller can run them once per cursor.
//! - A thin `InputState` method that applies the plan as a *single*
//!   replacement, which makes every command exactly one undo step.
//!
//! The bindings live in the [`CODE_EDITOR_CONTEXT`] key context, which the input only
//! declares while it is a multi-line code editor, so none of them reach plain
//! text fields.
mod guides;
mod lexer;
mod transform;

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;

use gpui::{App, Context, KeyBinding, SharedString, Window, actions};
use ropey::Rope;

use crate::input::{InputState, RopeExt as _, TabSize, mode::InputMode};
use lexer::Span;
use transform::Case;

actions!(
    input,
    [
        /// Comment or uncomment the selected lines with the language's line comment.
        ToggleLineComment,
        /// Wrap or unwrap the selection in the language's block comment.
        ToggleBlockComment,
        /// Move the selected lines up by one line.
        MoveLineUp,
        /// Move the selected lines down by one line.
        MoveLineDown,
        /// Duplicate the selected lines above, keeping the cursor on the upper copy.
        CopyLineUp,
        /// Duplicate the selected lines below, moving the cursor onto the lower copy.
        CopyLineDown,
        /// Delete the selected lines.
        DeleteLine,
        /// Open a new line below the cursor line, keeping its indent.
        InsertLineBelow,
        /// Open a new line above the cursor line, keeping its indent.
        InsertLineAbove,
        /// Select the cursor line, or extend a whole-line selection by one line.
        SelectLine,
        /// Jump to the bracket matching the one next to the cursor.
        MoveToMatchingBracket,
        /// Upper-case the selection, or the word under the cursor.
        TransformToUppercase,
        /// Lower-case the selection, or the word under the cursor.
        TransformToLowercase,
        /// Capitalize every word of the selection, or the word under the cursor.
        TransformToTitleCase,
        /// Remove trailing spaces and tabs from every line.
        TrimTrailingWhitespace,
        /// Join the selected lines, or the cursor line with the next one.
        JoinLines,
        /// Delete the innermost bracket pair around the selection.
        RemoveSurroundingBrackets,
    ]
);

/// The key context a multi-line code editor adds next to `Input`.
pub const CODE_EDITOR_CONTEXT: &str = "CodeEditor";

/// The whole key context of a multi-line code editor: `Input` plus
/// [`CODE_EDITOR_CONTEXT`].
pub(super) const INPUT_CODE_EDITOR_KEY_CONTEXT: &str = "Input CodeEditor";

/// How far a bracket match scans before giving up, in bytes.
const MAX_BRACKET_SCAN: usize = 64 * 1024;

/// Lines longer than this are not scanned for strings by the fallback
/// heuristic (minified files would make that quadratic).
const MAX_HEURISTIC_LINE: usize = 4 * 1024;

const BRACKETS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];

/// Characters an opener may be typed in front of and still get its closer.
const AUTO_CLOSE_BEFORE: &str = ";:.,=}])> \t\r\n";

pub(crate) fn init(cx: &mut App) {
    let context = Some(CODE_EDITOR_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("secondary-/", ToggleLineComment, context),
        KeyBinding::new("alt-shift-a", ToggleBlockComment, context),
        KeyBinding::new("alt-up", MoveLineUp, context),
        KeyBinding::new("alt-down", MoveLineDown, context),
        KeyBinding::new("alt-shift-up", CopyLineUp, context),
        KeyBinding::new("alt-shift-down", CopyLineDown, context),
        KeyBinding::new("secondary-shift-k", DeleteLine, context),
        KeyBinding::new("secondary-enter", InsertLineBelow, context),
        KeyBinding::new("secondary-shift-enter", InsertLineAbove, context),
        KeyBinding::new("secondary-l", SelectLine, context),
        KeyBinding::new("secondary-shift-\\", MoveToMatchingBracket, context),
    ]);
    // VS Code's ⌃J, which has no binding in the editor on macOS. Elsewhere
    // Ctrl+J is taken (VS Code's panel toggle), so it ships unbound there.
    #[cfg(target_os = "macos")]
    cx.bind_keys([KeyBinding::new("ctrl-j", JoinLines, context)]);
}

/// A pair whose closer was typed for the user, tracked so that only such a
/// closer is stepped over or deleted along with its opener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AutoClosedPair {
    /// Just after the opener.
    pub start: usize,
    /// The closer's offset.
    pub end: usize,
    pub closer: char,
}

/// Carry a tracked pair through an edit that replaced `range` with
/// `new_len` bytes. Edits inside the pair stretch it; an edit touching the
/// opener or the closer ends the tracking.
pub(crate) fn map_auto_closed(
    pair: AutoClosedPair,
    range: &Range<usize>,
    new_len: usize,
) -> Option<AutoClosedPair> {
    let delta = new_len as isize - range.len() as isize;
    let shift = |o: usize| (o as isize + delta) as usize;
    let opener = pair.start.checked_sub(1)?;
    // Entirely before the opener, an insertion right in front of it included.
    if range.end <= opener {
        return Some(AutoClosedPair {
            start: shift(pair.start),
            end: shift(pair.end),
            ..pair
        });
    }
    // Entirely after the closer.
    if range.start > pair.end {
        return Some(pair);
    }
    // Between the two, an insertion at either end of the inside included.
    if range.start >= pair.start && range.end <= pair.end {
        return Some(AutoClosedPair {
            end: shift(pair.end),
            ..pair
        });
    }
    None
}

/// Editing state the commands keep between keystrokes.
#[derive(Default)]
pub(crate) struct EditingState {
    auto_closed: Vec<AutoClosedPair>,
    /// Strings and comments found by [`lexer::scan`] for the current text and
    /// language, until the next edit.
    lexed: RefCell<Option<(SharedString, Rc<Vec<Span>>)>>,
    /// Inside a multi-cursor keystroke the scan is kept across the passes'
    /// edits instead of being redone for every cursor: it still holds for
    /// the text before the first byte any pass changed, which is this. The
    /// passes run last cursor first, so each asks about text before the
    /// edits already made.
    lexed_valid_before: Cell<Option<usize>>,
    /// How many times the whole text was scanned.
    #[cfg(test)]
    pub(crate) lex_count: Cell<usize>,
}

/// Texts larger than this are not lexed as a whole; the line-local guess
/// stands in.
const MAX_LEX_LEN: usize = 2 * 1024 * 1024;

/// One contiguous replacement plus the selection to put down after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditPlan {
    /// The replaced byte range, in the text before the edit.
    pub range: Range<usize>,
    pub new_text: String,
    /// The selection, in the text after the edit.
    pub selection: Range<usize>,
}

impl EditPlan {
    /// Apply to a plain string, for tests.
    #[cfg(test)]
    fn apply(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len() + self.new_text.len());
        out.push_str(&text[..self.range.start]);
        out.push_str(&self.new_text);
        out.push_str(&text[self.range.end..]);
        out
    }
}

/// A line command's edit over every selection at once: one contiguous
/// replacement, and each selection (in the order they were passed) after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinesPlan {
    /// The replaced byte range, in the text before the edit.
    pub range: Range<usize>,
    pub new_text: String,
    /// The selections, in the text after the edit.
    pub selections: Vec<Range<usize>>,
}

impl LinesPlan {
    #[cfg(test)]
    fn apply(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len() + self.new_text.len());
        out.push_str(&text[..self.range.start]);
        out.push_str(&self.new_text);
        out.push_str(&text[self.range.end..]);
        out
    }
}

/// What typing a character turned into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Typed {
    /// Replace text, e.g. `(` became `()` or wrapped the selection.
    Edit(EditPlan),
    /// Only step the cursor over a closer that is already there.
    Skip(usize),
}

/// Per-language knobs for the editing commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LanguageConfig {
    pub line_comment: Option<&'static str>,
    pub block_comment: Option<(&'static str, &'static str)>,
    /// Quote characters that auto-close and delimit strings.
    pub quotes: &'static [char],
    /// Whether `"` and `'` strings may run across lines (backtick ones
    /// always may).
    pub multi_line_strings: bool,
}

impl LanguageConfig {
    const PLAIN: Self = Self {
        line_comment: None,
        block_comment: None,
        quotes: &[],
        multi_line_strings: false,
    };

    fn pairs_quote(&self, c: char) -> bool {
        self.quotes.contains(&c)
    }
}

/// Look up the editing config for a code editor language name.
///
/// The names are the ones handed to `InputState::code_editor` (the highlighter
/// registry names), plus a few common aliases.
pub(crate) fn language_config(language: &str) -> LanguageConfig {
    const C_BLOCK: Option<(&str, &str)> = Some(("/*", "*/"));
    const XML_BLOCK: Option<(&str, &str)> = Some(("<!--", "-->"));
    const ALL_QUOTES: &[char] = &['"', '\'', '`'];
    const DQ_SQ: &[char] = &['"', '\''];
    const DQ: &[char] = &['"'];

    let c_like = |quotes| LanguageConfig {
        line_comment: Some("//"),
        block_comment: C_BLOCK,
        quotes,
        multi_line_strings: false,
    };
    let hash = |quotes| LanguageConfig {
        line_comment: Some("#"),
        block_comment: None,
        quotes,
        multi_line_strings: false,
    };

    match language.to_ascii_lowercase().as_str() {
        // Single quotes are lifetimes far more often than char literals.
        "rust" | "rs" => LanguageConfig {
            multi_line_strings: true,
            ..c_like(DQ)
        },
        "javascript" | "js" | "jsx" | "typescript" | "ts" | "tsx" | "mjs" | "cjs" => {
            c_like(ALL_QUOTES)
        }
        "go" | "kotlin" | "kt" | "scala" | "dart" | "groovy" => c_like(ALL_QUOTES),
        "c" | "cpp" | "c++" | "objc" | "java" | "csharp" | "c#" | "cs" | "swift" | "proto"
        | "protobuf" | "php" | "scss" | "less" => c_like(DQ_SQ),
        "zig" => LanguageConfig {
            line_comment: Some("//"),
            block_comment: None,
            quotes: DQ_SQ,
            multi_line_strings: false,
        },
        "json" | "jsonc" | "json5" => c_like(DQ),
        "python" | "py" | "ruby" | "rb" | "perl" | "r" | "nix" | "elixir" | "ex" | "exs"
        | "dockerfile" | "conf" | "ini" => hash(DQ_SQ),
        "bash" | "sh" | "shell" | "zsh" | "fish" => LanguageConfig {
            multi_line_strings: true,
            ..hash(ALL_QUOTES)
        },
        "make" | "makefile" | "cmake" => hash(ALL_QUOTES),
        "toml" | "yaml" | "yml" | "graphql" | "gql" => hash(DQ_SQ),
        "sql" | "haskell" | "hs" | "elm" => LanguageConfig {
            line_comment: Some("--"),
            block_comment: C_BLOCK,
            quotes: DQ_SQ,
            multi_line_strings: false,
        },
        "lua" => LanguageConfig {
            line_comment: Some("--"),
            block_comment: Some(("--[[", "]]")),
            quotes: DQ_SQ,
            multi_line_strings: false,
        },
        "lisp" | "clojure" | "scheme" | "elisp" => LanguageConfig {
            line_comment: Some(";"),
            block_comment: None,
            quotes: DQ,
            multi_line_strings: false,
        },
        "erlang" | "latex" | "tex" => LanguageConfig {
            line_comment: Some("%"),
            block_comment: None,
            quotes: DQ,
            multi_line_strings: false,
        },
        "css" => LanguageConfig {
            line_comment: None,
            block_comment: C_BLOCK,
            quotes: DQ_SQ,
            multi_line_strings: false,
        },
        "html" | "htm" | "xml" | "svg" | "vue" | "svelte" | "astro" | "erb" | "ejs" => {
            LanguageConfig {
                line_comment: None,
                block_comment: XML_BLOCK,
                quotes: DQ_SQ,
                multi_line_strings: false,
            }
        }
        // Prose: an apostrophe or a quote mark should never grow a twin.
        "markdown" | "md" => LanguageConfig {
            line_comment: None,
            block_comment: XML_BLOCK,
            quotes: &['`'],
            multi_line_strings: false,
        },
        "text" | "plain" | "plaintext" | "diff" | "" => LanguageConfig::PLAIN,
        _ => LanguageConfig {
            line_comment: None,
            block_comment: None,
            quotes: DQ_SQ,
            multi_line_strings: false,
        },
    }
}

// ---------------------------------------------------------------------------
// Small text helpers
// ---------------------------------------------------------------------------

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn char_before(text: &Rope, offset: usize) -> Option<char> {
    if offset == 0 || offset > text.len() {
        return None;
    }
    text.chars_at(offset).reversed().next()
}

fn char_after(text: &Rope, offset: usize) -> Option<char> {
    text.char_at(offset)
}

fn closer_for(open: char) -> Option<char> {
    BRACKETS.iter().find(|(o, _)| *o == open).map(|(_, c)| *c)
}

fn is_closer(c: char) -> bool {
    BRACKETS.iter().any(|(_, close)| *close == c)
}

fn line_string(text: &Rope, row: usize) -> String {
    text.slice_line(row).to_string()
}

/// A line without its trailing `\r`, if it has one.
fn without_cr(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

fn leading_whitespace(line: &str) -> &str {
    let line = without_cr(line);
    let end = line
        .find(|c: char| c != ' ' && c != '\t')
        .unwrap_or(line.len());
    &line[..end]
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// The visual column of the first non-blank character, with tabs advancing to
/// the next tab stop.
fn indent_columns(line: &str, tab: TabSize) -> usize {
    let mut col = 0;
    for c in leading_whitespace(line).chars() {
        col = advance_column(col, c, tab);
    }
    col
}

fn advance_column(col: usize, c: char, tab: TabSize) -> usize {
    if c == '\t' {
        let size = tab.tab_size.max(1);
        col + size - col % size
    } else {
        col + 1
    }
}

/// The byte index in `line` where the visual column `target` is reached,
/// never past the leading whitespace.
fn byte_at_column(line: &str, target: usize, tab: TabSize) -> usize {
    let mut col = 0;
    for (ix, c) in leading_whitespace(line).char_indices() {
        if col >= target {
            return ix;
        }
        col = advance_column(col, c, tab);
    }
    leading_whitespace(line).len()
}

/// The rows a selection covers, end exclusive. A selection that ends at the
/// very start of a line does not take that line along.
pub(crate) fn selected_rows(text: &Rope, selection: &Range<usize>) -> Range<usize> {
    let start = text.offset_to_point(selection.start).row;
    let mut end = text.offset_to_point(selection.end).row;
    if selection.end > selection.start
        && end > start
        && text.line_start_offset(end) == selection.end
    {
        end -= 1;
    }
    start..end + 1
}

/// Fold a list of sorted, non-overlapping edits into one replacement.
fn compose_text(text: &Rope, edits: &[(Range<usize>, String)]) -> Option<(Range<usize>, String)> {
    let first = edits.first()?;
    let last = edits.last()?;
    let covering = first.0.start..last.0.end;

    let mut new_text = String::new();
    let mut cursor = covering.start;
    for (range, insert) in edits {
        new_text.push_str(&text.slice(cursor..range.start).to_string());
        new_text.push_str(insert);
        cursor = range.end;
    }
    Some((covering, new_text))
}

/// Carry a selection through `edits`. A bare cursor at an insertion point
/// moves past the insertion; a selection grows to take in text inserted at
/// either of its ends.
fn map_selection(edits: &[(Range<usize>, String)], selection: &Range<usize>) -> Range<usize> {
    let start = map_offset(edits, selection.start, selection.is_empty());
    let end = map_offset(edits, selection.end, true);
    start..end
}

/// [`compose_text`] into an [`EditPlan`], carrying `selection` along.
fn compose(
    text: &Rope,
    edits: &[(Range<usize>, String)],
    selection: &Range<usize>,
) -> Option<EditPlan> {
    let (range, new_text) = compose_text(text, edits)?;
    Some(EditPlan {
        range,
        new_text,
        selection: map_selection(edits, selection),
    })
}

/// [`compose_text`] into a [`LinesPlan`], carrying every selection along.
fn compose_lines(
    text: &Rope,
    edits: &[(Range<usize>, String)],
    selections: &[Range<usize>],
) -> Option<LinesPlan> {
    let (range, new_text) = compose_text(text, edits)?;
    Some(LinesPlan {
        range,
        new_text,
        selections: selections.iter().map(|s| map_selection(edits, s)).collect(),
    })
}

/// Every row any selection touches, sorted, each once.
fn union_rows(text: &Rope, selections: &[Range<usize>]) -> Vec<usize> {
    let rows: std::collections::BTreeSet<usize> = selections
        .iter()
        .flat_map(|s| selected_rows(text, s))
        .collect();
    rows.into_iter().collect()
}

/// The selections' rows as contiguous blocks (overlapping or adjacent row
/// ranges merge, so two cursors on neighbouring lines move together), and for
/// each selection the index of its block.
fn row_blocks(text: &Rope, selections: &[Range<usize>]) -> (Vec<Range<usize>>, Vec<usize>) {
    let mut rows: Vec<(Range<usize>, usize)> = selections
        .iter()
        .enumerate()
        .map(|(ix, s)| (selected_rows(text, s), ix))
        .collect();
    rows.sort_by_key(|(r, _)| (r.start, r.end));

    let mut blocks: Vec<Range<usize>> = Vec::new();
    let mut owner = vec![0; selections.len()];
    for (range, ix) in rows {
        match blocks.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => blocks.push(range),
        }
        owner[ix] = blocks.len() - 1;
    }
    (blocks, owner)
}

/// Where `offset` lands after `edits` are applied. An offset at an insertion
/// point moves past the inserted text when `stick_right`, and stays in front
/// of it otherwise; one inside a replaced range is clamped into the
/// replacement.
fn map_offset(edits: &[(Range<usize>, String)], offset: usize, stick_right: bool) -> usize {
    let mut delta: isize = 0;
    for (range, insert) in edits {
        if offset < range.start || (range.is_empty() && offset == range.start && !stick_right) {
            break;
        }
        if offset >= range.end {
            delta += insert.len() as isize - range.len() as isize;
        } else {
            // Strictly inside a replaced range.
            let inside = (offset - range.start).min(insert.len());
            return (range.start as isize + delta) as usize + inside;
        }
    }
    (offset as isize + delta) as usize
}

// ---------------------------------------------------------------------------
// Comments
// ---------------------------------------------------------------------------

/// Toggle a line comment over the selected lines.
///
/// Comments every non-blank line at the smallest indent among them, unless
/// every non-blank line is already commented, in which case it uncomments.
/// A selection of nothing but blank lines gets the token after its indent.
pub(crate) fn toggle_line_comment(
    text: &Rope,
    selections: &[Range<usize>],
    token: &str,
    tab: TabSize,
) -> Option<LinesPlan> {
    let lines: Vec<(usize, String)> = union_rows(text, selections)
        .into_iter()
        .map(|row| (row, line_string(text, row)))
        .collect();
    let non_blank: Vec<&(usize, String)> = lines.iter().filter(|(_, l)| !is_blank(l)).collect();

    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    if non_blank.is_empty() {
        for (row, line) in &lines {
            let at = text.line_start_offset(*row) + leading_whitespace(line).len();
            edits.push((at..at, format!("{token} ")));
        }
    } else if non_blank
        .iter()
        .all(|(_, l)| l[leading_whitespace(l).len()..].starts_with(token))
    {
        for (row, line) in non_blank {
            let at = text.line_start_offset(*row) + leading_whitespace(line).len();
            let rest = &line[leading_whitespace(line).len() + token.len()..];
            let len = token.len() + usize::from(rest.starts_with(' '));
            edits.push((at..at + len, String::new()));
        }
    } else {
        let min_indent = non_blank
            .iter()
            .map(|(_, l)| indent_columns(l, tab))
            .min()
            .unwrap_or(0);
        for (row, line) in non_blank {
            let at = text.line_start_offset(*row) + byte_at_column(line, min_indent, tab);
            edits.push((at..at, format!("{token} ")));
        }
    }

    compose_lines(text, &edits, selections)
}

/// Toggle a block comment around each selected line, for languages that have
/// no line comment (CSS, HTML, Markdown).
pub(crate) fn toggle_line_block_comment(
    text: &Rope,
    selections: &[Range<usize>],
    open: &str,
    close: &str,
) -> Option<LinesPlan> {
    let lines: Vec<(usize, String)> = union_rows(text, selections)
        .into_iter()
        .map(|row| (row, line_string(text, row)))
        .filter(|(_, l)| !is_blank(l))
        .collect();
    if lines.is_empty() {
        // Only blank lines: an empty comment at each cursor, cursor inside.
        let insert = format!("{open}  {close}");
        let edits: Vec<(Range<usize>, String)> = selections
            .iter()
            .map(|s| (s.clone(), insert.clone()))
            .collect();
        let (range, new_text) = compose_text(text, &edits)?;
        let mut delta = 0isize;
        let selections = selections
            .iter()
            .map(|s| {
                let caret = (s.start as isize + delta) as usize + open.len() + 1;
                delta += insert.len() as isize - s.len() as isize;
                caret..caret
            })
            .collect();
        return Some(LinesPlan {
            range,
            new_text,
            selections,
        });
    }

    let commented = lines.iter().all(|(_, l)| {
        // The same indent the edits below measure: `trim` would also strip
        // non-ASCII spaces (U+3000), and the offsets would then disagree.
        let t = without_cr(l)[leading_whitespace(l).len()..].trim_end();
        t.starts_with(open) && t.ends_with(close) && t.len() >= open.len() + close.len()
    });

    let mut edits = Vec::new();
    for (row, line) in &lines {
        let line_start = text.line_start_offset(*row);
        let content = without_cr(line);
        let start = leading_whitespace(line).len();
        let end = content.trim_end().len();
        if commented {
            let inner = &content[start + open.len()..end - close.len()];
            let lead = usize::from(inner.starts_with(' '));
            let trail = usize::from(inner.len() > lead && inner.ends_with(' '));
            let open_end = line_start + start + open.len() + lead;
            let close_start = line_start + end - close.len() - trail;
            edits.push((line_start + start..open_end, String::new()));
            edits.push((close_start..line_start + end, String::new()));
        } else {
            edits.push((line_start + start..line_start + start, format!("{open} ")));
            edits.push((line_start + end..line_start + end, format!(" {close}")));
        }
    }

    compose_lines(text, &edits, selections)
}

/// Wrap the selection in a block comment, or unwrap it if it already is one.
///
/// An empty selection works on the trimmed content of its line; on a blank
/// line it inserts an empty comment with the cursor inside.
pub(crate) fn toggle_block_comment(
    text: &Rope,
    selection: &Range<usize>,
    open: &str,
    close: &str,
) -> Option<EditPlan> {
    let range = if selection.is_empty() {
        let row = text.offset_to_point(selection.start).row;
        let line = line_string(text, row);
        if is_blank(&line) {
            let plan =
                toggle_line_block_comment(text, std::slice::from_ref(selection), open, close)?;
            return Some(EditPlan {
                range: plan.range,
                new_text: plan.new_text,
                selection: plan.selections[0].clone(),
            });
        }
        let line_start = text.line_start_offset(row);
        let start = leading_whitespace(&line).len();
        let end = without_cr(&line).trim_end().len();
        line_start + start..line_start + end
    } else {
        selection.clone()
    };

    let body = text.slice(range.clone()).to_string();
    let trimmed = body.trim();
    if trimmed.starts_with(open)
        && trimmed.ends_with(close)
        && trimmed.len() >= open.len() + close.len()
    {
        let start = range.start + (body.len() - body.trim_start().len());
        let end = range.start + body.trim_end().len();
        let inner = &text
            .slice(start + open.len()..end - close.len())
            .to_string();
        let lead = usize::from(inner.starts_with(' '));
        let trail = usize::from(inner.len() > lead && inner.ends_with(' '));
        let edits = [
            (start..start + open.len() + lead, String::new()),
            (end - close.len() - trail..end, String::new()),
        ];
        return compose(text, &edits, selection);
    }

    let edits = [
        (range.start..range.start, format!("{open} ")),
        (range.end..range.end, format!(" {close}")),
    ];
    let mut plan = compose(text, &edits, selection)?;
    if !selection.is_empty() {
        // Keep the selection on what was selected, inside the markers.
        plan.selection = selection.start + open.len() + 1..selection.end + open.len() + 1;
    }
    Some(plan)
}

// ---------------------------------------------------------------------------
// Line operations
// ---------------------------------------------------------------------------

fn shift_range(range: &Range<usize>, delta: isize) -> Range<usize> {
    let shift = |o: usize| (o as isize + delta).max(0) as usize;
    shift(range.start)..shift(range.end)
}

/// Swap each block of selected lines with the line above (`up`) or below.
/// Nothing moves when any block is already at that end of the text.
pub(crate) fn move_lines(text: &Rope, selections: &[Range<usize>], up: bool) -> Option<LinesPlan> {
    let (blocks, owner) = row_blocks(text, selections);
    let mut edits = Vec::with_capacity(blocks.len());
    let mut deltas = Vec::with_capacity(blocks.len());
    for rows in &blocks {
        let block = text.slice_lines(rows.clone()).to_string();
        if up {
            let prev = rows.start.checked_sub(1)?;
            let prev_line = line_string(text, prev);
            let range = text.line_start_offset(prev)..text.line_end_offset(rows.end - 1);
            edits.push((range, format!("{block}\n{prev_line}")));
            deltas.push(-(prev_line.len() as isize + 1));
        } else {
            let next = rows.end;
            if next >= text.lines_len() {
                return None;
            }
            let next_line = line_string(text, next);
            let range = text.line_start_offset(rows.start)..text.line_end_offset(next);
            edits.push((range, format!("{next_line}\n{block}")));
            deltas.push(next_line.len() as isize + 1);
        }
    }
    let (range, new_text) = compose_text(text, &edits)?;
    let selections = selections
        .iter()
        .zip(&owner)
        .map(|(s, &block)| shift_range(s, deltas[block]))
        .collect();
    Some(LinesPlan {
        range,
        new_text,
        selections,
    })
}

/// Duplicate each block of selected lines. Copying down moves the selections
/// onto the new lower copy; copying up leaves them on the new upper copy.
pub(crate) fn copy_lines(text: &Rope, selections: &[Range<usize>], up: bool) -> Option<LinesPlan> {
    let (blocks, owner) = row_blocks(text, selections);
    let mut edits = Vec::with_capacity(blocks.len());
    for rows in &blocks {
        let block = text.slice_lines(rows.clone()).to_string();
        if up {
            let at = text.line_start_offset(rows.start);
            edits.push((at..at, format!("{block}\n")));
        } else {
            let at = text.line_end_offset(rows.end - 1);
            edits.push((at..at, format!("\n{block}")));
        }
    }
    let (range, new_text) = compose_text(text, &edits)?;
    let selections = selections
        .iter()
        .zip(&owner)
        .map(|(s, &block)| {
            let before: usize = edits[..block].iter().map(|(_, t)| t.len()).sum();
            let own = if up { 0 } else { edits[block].1.len() };
            shift_range(s, (before + own) as isize)
        })
        .collect();
    Some(LinesPlan {
        range,
        new_text,
        selections,
    })
}

/// The byte index of the `chars`-th character of `line`, clamped to its end.
fn byte_for_char_column(line: &str, chars: usize) -> usize {
    let line = without_cr(line);
    line.char_indices()
        .nth(chars)
        .map(|(ix, _)| ix)
        .unwrap_or(line.len())
}

/// Delete every block of selected lines. Each cursor (`heads[i]` is the caret
/// of `selections[i]`) lands on the line that takes its block's place, at the
/// same character column.
pub(crate) fn delete_lines(
    text: &Rope,
    selections: &[Range<usize>],
    heads: &[usize],
) -> Option<LinesPlan> {
    let (blocks, owner) = row_blocks(text, selections);
    let total = text.lines_len();
    let ranges: Vec<Range<usize>> = blocks
        .iter()
        .map(|rows| {
            if rows.end < total {
                text.line_start_offset(rows.start)..text.line_start_offset(rows.end)
            } else if rows.start > 0 {
                text.line_end_offset(rows.start - 1)..text.len()
            } else {
                0..text.len()
            }
        })
        .collect();
    let edits: Vec<(Range<usize>, String)> =
        ranges.iter().map(|r| (r.clone(), String::new())).collect();
    let (range, new_text) = compose_text(text, &edits)?;

    let selections = heads
        .iter()
        .zip(&owner)
        .map(|(&head, &block)| {
            let rows = &blocks[block];
            let row = text.offset_to_point(head).row;
            let column = text
                .slice(text.line_start_offset(row)..head)
                .chars()
                .count();
            let target_row = if rows.end < total {
                Some(rows.end)
            } else {
                rows.start.checked_sub(1)
            };
            let caret = match target_row {
                Some(row) => {
                    text.line_start_offset(row)
                        + byte_for_char_column(&line_string(text, row), column)
                }
                None => 0,
            };
            let deleted: usize = ranges
                .iter()
                .filter(|r| r.end <= caret && !r.is_empty())
                .map(|r| r.len())
                .sum();
            let caret = caret.saturating_sub(deleted);
            caret..caret
        })
        .collect();
    Some(LinesPlan {
        range,
        new_text,
        selections,
    })
}

/// Open an empty line below (or above) the cursor's line with the same
/// indent, and put the cursor on it.
pub(crate) fn insert_line(text: &Rope, cursor: usize, above: bool) -> EditPlan {
    let row = text.offset_to_point(cursor).row;
    let line = line_string(text, row);
    let indent = leading_whitespace(&line).to_string();
    let eol = if line.ends_with('\r') { "\r\n" } else { "\n" };
    if above {
        let at = text.line_start_offset(row);
        let caret = at + indent.len();
        EditPlan {
            range: at..at,
            new_text: format!("{indent}{eol}"),
            selection: caret..caret,
        }
    } else {
        let at = text.line_start_offset(row) + without_cr(&line).len();
        let caret = at + eol.len() + indent.len();
        EditPlan {
            range: at..at,
            new_text: format!("{eol}{indent}"),
            selection: caret..caret,
        }
    }
}

/// Open an empty line below (or above) the line of every cursor, once per
/// line however many cursors are on it. Each cursor lands on its line's new
/// line.
pub(crate) fn insert_lines(text: &Rope, heads: &[usize], above: bool) -> Option<LinesPlan> {
    let head_rows: Vec<usize> = heads
        .iter()
        .map(|&head| text.offset_to_point(head).row)
        .collect();
    let mut rows = head_rows.clone();
    rows.sort_unstable();
    rows.dedup();
    let plans: Vec<EditPlan> = rows
        .iter()
        .map(|&row| insert_line(text, text.line_start_offset(row), above))
        .collect();
    let edits: Vec<(Range<usize>, String)> = plans
        .iter()
        .map(|plan| (plan.range.clone(), plan.new_text.clone()))
        .collect();
    let (range, new_text) = compose_text(text, &edits)?;
    let selections = head_rows
        .iter()
        .map(|row| {
            let k = rows.binary_search(row).unwrap_or(0);
            let before: usize = edits[..k].iter().map(|(_, t)| t.len()).sum();
            let caret = plans[k].selection.start + before;
            caret..caret
        })
        .collect();
    Some(LinesPlan {
        range,
        new_text,
        selections,
    })
}

/// The whole lines under the selection, newline included. A selection that
/// already is exactly that grows by the next line.
pub(crate) fn select_lines(text: &Rope, selection: &Range<usize>) -> Range<usize> {
    let rows = selected_rows(text, selection);
    let line_end = |row: usize| {
        if row < text.lines_len() {
            text.line_start_offset(row)
        } else {
            text.len()
        }
    };
    let start = text.line_start_offset(rows.start);
    let end = line_end(rows.end);
    if selection.start == start && selection.end == end && !selection.is_empty() {
        return start..line_end(rows.end + 1);
    }
    start..end
}

// ---------------------------------------------------------------------------
// Typing: pairs and Enter
// ---------------------------------------------------------------------------

/// What typing `typed` over `selection` should do instead of a plain insert.
///
/// `in_string` says whether the cursor sits inside a string or comment; it
/// only suppresses quote pairing, as brackets are paired everywhere.
/// `overtype` says the character right after the cursor is a closer that was
/// auto-inserted; only such a closer is stepped over.
pub(crate) fn auto_pair(
    text: &Rope,
    selection: &Range<usize>,
    typed: &str,
    config: &LanguageConfig,
    in_string: bool,
    overtype: bool,
) -> Option<Typed> {
    let mut chars = typed.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let is_quote = matches!(c, '"' | '\'' | '`');
    let closer = closer_for(c);

    if !selection.is_empty() {
        let close = closer.or_else(|| (is_quote && config.pairs_quote(c)).then_some(c))?;
        let selected = text.slice(selection.clone()).to_string();
        return Some(Typed::Edit(EditPlan {
            range: selection.clone(),
            new_text: format!("{c}{selected}{close}"),
            selection: selection.start + 1..selection.end + 1,
        }));
    }

    let at = selection.start;
    let next = char_after(text, at);

    // Step over a closer that was typed for the user.
    if overtype && next == Some(c) && (is_closer(c) || is_quote) {
        return Some(Typed::Skip(at + c.len_utf8()));
    }

    let next_allows = next.is_none_or(|n| AUTO_CLOSE_BEFORE.contains(n));
    if let Some(close) = closer {
        if !next_allows {
            return None;
        }
        return Some(Typed::Edit(EditPlan {
            range: at..at,
            new_text: format!("{c}{close}"),
            selection: at + 1..at + 1,
        }));
    }

    if is_quote && config.pairs_quote(c) && !in_string && next_allows {
        // `don't`, `x'`: a quote right after a word is an apostrophe or a
        // closing quote, never the start of a pair.
        if char_before(text, at).is_some_and(|p| is_word_char(p) || p == c) {
            return None;
        }
        return Some(Typed::Edit(EditPlan {
            range: at..at,
            new_text: format!("{c}{c}"),
            selection: at + 1..at + 1,
        }));
    }

    None
}

/// The range Backspace should delete when the cursor sits inside an empty
/// pair: `(|)`, `[|]`, `{|}`, `"|"`, `'|'` or `` `|` ``.
pub(crate) fn backspace_pair(text: &Rope, cursor: usize) -> Option<Range<usize>> {
    let prev = char_before(text, cursor)?;
    let next = char_after(text, cursor)?;
    let paired =
        closer_for(prev) == Some(next) || (matches!(prev, '"' | '\'' | '`') && prev == next);
    paired.then(|| cursor - prev.len_utf8()..cursor + next.len_utf8())
}

/// Enter after an opening bracket: indent one level, and when the matching
/// closer follows (`{|}`), push it onto its own line below.
pub(crate) fn smart_newline(text: &Rope, cursor: usize, tab: TabSize) -> Option<EditPlan> {
    let row = text.offset_to_point(cursor).row;
    let line_start = text.line_start_offset(row);
    let line = line_string(text, row);
    let line = without_cr(&line);
    let column = cursor - line_start;
    let (before, after) = line.split_at(column.min(line.len()));

    let trimmed_before = before.trim_end_matches([' ', '\t']);
    let open = trimmed_before.chars().next_back()?;
    let close = closer_for(open)?;
    let indent = leading_whitespace(line);
    let unit = tab.to_string();

    let trimmed_after = after.trim_start_matches([' ', '\t']);
    let start = line_start + trimmed_before.len();
    if trimmed_after.starts_with(close) {
        let end = cursor + (after.len() - trimmed_after.len());
        let new_text = format!("\n{indent}{unit}\n{indent}");
        let caret = start + 1 + indent.len() + unit.len();
        return Some(EditPlan {
            range: start..end,
            new_text,
            selection: caret..caret,
        });
    }

    // Only when nothing but whitespace follows; `foo(|bar)` is left alone.
    if !trimmed_after.is_empty() {
        return None;
    }
    let new_text = format!("\n{indent}{unit}");
    let caret = start + new_text.len();
    Some(EditPlan {
        range: start..cursor + after.len(),
        new_text,
        selection: caret..caret,
    })
}

// ---------------------------------------------------------------------------
// Strings and brackets
// ---------------------------------------------------------------------------

/// Whether `prefix` (a line up to the cursor) ends inside a string or a line
/// comment. A line-local stand-in for when there is no syntax tree.
pub(crate) fn prefix_in_string_or_comment(prefix: &str, config: &LanguageConfig) -> bool {
    if prefix.len() > MAX_HEURISTIC_LINE {
        return false;
    }
    let mut quote: Option<char> = None;
    let mut ix = 0;
    while let Some(c) = prefix[ix..].chars().next() {
        let rest = &prefix[ix..];
        match quote {
            Some(q) => {
                if c == '\\' {
                    ix += 1;
                    ix += prefix[ix..].chars().next().map_or(0, char::len_utf8);
                    continue;
                }
                if c == q {
                    quote = None;
                }
            }
            None => {
                if config
                    .line_comment
                    .is_some_and(|token| rest.starts_with(token))
                {
                    return true;
                }
                if let Some((open, close)) = config.block_comment
                    && rest.starts_with(open)
                {
                    match rest[open.len()..].find(close) {
                        Some(at) => {
                            ix += open.len() + at + close.len();
                            continue;
                        }
                        None => return true,
                    }
                }
                if config.pairs_quote(c) {
                    quote = Some(c);
                }
            }
        }
        ix += c.len_utf8();
    }
    quote.is_some()
}

fn is_string_or_comment_kind(kind: &str) -> bool {
    kind.contains("string")
        || kind.contains("comment")
        || kind == "char_literal"
        || kind == "character"
        || kind == "rune_literal"
}

/// Whether the byte at `offset` is part of a string or comment node.
pub(crate) fn tree_offset_in_string_or_comment(tree: &tree_sitter::Tree, offset: usize) -> bool {
    let Some(mut node) = tree
        .root_node()
        .descendant_for_byte_range(offset, offset + 1)
    else {
        return false;
    };
    loop {
        if is_string_or_comment_kind(node.kind()) {
            return true;
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return false,
        }
    }
}

/// Whether a cursor at `offset` is strictly inside a string, or inside a
/// comment (whose end it may touch).
pub(crate) fn tree_cursor_in_string_or_comment(tree: &tree_sitter::Tree, offset: usize) -> bool {
    if offset == 0 {
        return false;
    }
    let Some(mut node) = tree
        .root_node()
        .descendant_for_byte_range(offset - 1, offset)
    else {
        return false;
    };
    loop {
        let kind = node.kind();
        if is_string_or_comment_kind(kind) {
            let (start, end) = (node.start_byte(), node.end_byte());
            let inside = if kind.contains("comment") {
                start < offset && offset <= end
            } else {
                start < offset && offset < end
            };
            if inside {
                return true;
            }
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return false,
        }
    }
}

/// The bracket next to `cursor` and its match, as the byte offsets of both
/// bracket characters (in text order).
///
/// A closer just before the cursor wins, then any bracket just after it, then
/// an opener just before it. `skip` reports offsets inside strings and
/// comments: brackets there neither match nor count.
pub(crate) fn bracket_pair_at(
    text: &Rope,
    cursor: usize,
    skip: &dyn Fn(usize) -> bool,
) -> Option<(usize, usize)> {
    let before = char_before(text, cursor).map(|c| (cursor - c.len_utf8(), c));
    let after = char_after(text, cursor).map(|c| (cursor, c));

    let mut candidates = Vec::with_capacity(3);
    if let Some((pos, c)) = before
        && is_closer(c)
    {
        candidates.push(pos);
    }
    if let Some((pos, c)) = after
        && (is_closer(c) || closer_for(c).is_some())
    {
        candidates.push(pos);
    }
    if let Some((pos, c)) = before
        && closer_for(c).is_some()
    {
        candidates.push(pos);
    }

    candidates.into_iter().find_map(|pos| {
        if skip(pos) {
            return None;
        }
        let other = matching_bracket(text, pos, skip)?;
        Some((pos.min(other), pos.max(other)))
    })
}

/// The offset of the bracket matching the one at `pos`.
pub(crate) fn matching_bracket(
    text: &Rope,
    pos: usize,
    skip: &dyn Fn(usize) -> bool,
) -> Option<usize> {
    let c = char_after(text, pos)?;
    if let Some(close) = closer_for(c) {
        let mut depth = 0usize;
        let mut offset = pos + c.len_utf8();
        for ch in text.chars_at(offset) {
            if offset - pos > MAX_BRACKET_SCAN {
                return None;
            }
            if (ch == c || ch == close) && !skip(offset) {
                if ch == close {
                    if depth == 0 {
                        return Some(offset);
                    }
                    depth -= 1;
                } else {
                    depth += 1;
                }
            }
            offset += ch.len_utf8();
        }
        None
    } else {
        let open = BRACKETS.iter().find(|(_, cl)| *cl == c)?.0;
        let mut depth = 0usize;
        let mut offset = pos;
        for ch in text.chars_at(pos).reversed() {
            offset -= ch.len_utf8();
            if pos - offset > MAX_BRACKET_SCAN {
                return None;
            }
            if (ch == c || ch == open) && !skip(offset) {
                if ch == open {
                    if depth == 0 {
                        return Some(offset);
                    }
                    depth -= 1;
                } else {
                    depth += 1;
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// InputState glue
// ---------------------------------------------------------------------------

impl InputState {
    /// Whether the editing commands apply: a multi-line, editable code editor
    /// with no IME composition in flight.
    fn editing_commands_enabled(&self) -> bool {
        self.mode.is_code_editor()
            && self.mode.is_multi_line()
            && !self.disabled
            && self.ime_marked_range.is_none()
    }

    fn editing_language(&self) -> LanguageConfig {
        match &self.mode {
            InputMode::CodeEditor { language, .. } => language_config(language),
            _ => LanguageConfig::PLAIN,
        }
    }

    /// Run `f` with the current syntax tree, if the highlighter has one for
    /// the current text.
    fn with_syntax_tree<R>(&self, f: impl FnOnce(&tree_sitter::Tree) -> R) -> Option<R> {
        let highlighter = self.mode.highlighter()?.borrow();
        let highlighter = highlighter.as_ref()?;
        if highlighter.text().len() != self.text.len() {
            return None;
        }
        highlighter.tree().map(f)
    }

    /// Strings and comments by [`lexer::scan`], for a text with no syntax
    /// tree; cached until the next edit.
    fn lexed_spans(&self, config: &LanguageConfig) -> Option<Rc<Vec<Span>>> {
        if self.text.len() > MAX_LEX_LEN {
            return None;
        }
        let language = match &self.mode {
            InputMode::CodeEditor { language, .. } => language.clone(),
            _ => return None,
        };
        let mut lexed = self.editing.lexed.borrow_mut();
        if let Some((cached_for, spans)) = lexed.as_ref()
            && *cached_for == language
            && self.editing.lexed_valid_before.get().is_none()
        {
            return Some(spans.clone());
        }
        #[cfg(test)]
        self.editing
            .lex_count
            .set(self.editing.lex_count.get() + 1);
        let spans = Rc::new(lexer::scan(&self.text.to_string(), config));
        *lexed = Some((language, spans.clone()));
        self.editing.lexed_valid_before.set(None);
        Some(spans)
    }

    /// [`Self::lexed_spans`] for a question about `offset` alone: a scan
    /// kept through this keystroke's edits answers it while `offset` comes
    /// before all of them.
    fn lexed_spans_for(&self, offset: usize, config: &LanguageConfig) -> Option<Rc<Vec<Span>>> {
        if let Some(valid_before) = self.editing.lexed_valid_before.get()
            && offset < valid_before
            && let InputMode::CodeEditor { language, .. } = &self.mode
            && let Some((cached_for, spans)) = self.editing.lexed.borrow().as_ref()
            && cached_for == language
        {
            return Some(spans.clone());
        }
        self.lexed_spans(config)
    }

    fn cursor_in_string_or_comment(&self, offset: usize, config: &LanguageConfig) -> bool {
        if let Some(inside) =
            self.with_syntax_tree(|tree| tree_cursor_in_string_or_comment(tree, offset))
        {
            return inside;
        }
        if let Some(spans) = self.lexed_spans_for(offset, config) {
            return lexer::cursor_in_span(&spans, offset);
        }
        let row = self.text.offset_to_point(offset).row;
        let prefix = self
            .text
            .slice(self.text.line_start_offset(row)..offset)
            .to_string();
        prefix_in_string_or_comment(&prefix, config)
    }

    /// Apply one cursor's `plan`. Callers bracket the whole command with
    /// [`crate::history::History::break_group`] so it is one undo step.
    fn apply_edit_plan(&mut self, plan: EditPlan, window: &mut Window, cx: &mut Context<Self>) {
        let range_utf16 = self.range_to_utf16(&plan.range);
        self.replace_text_in_range_silent(Some(range_utf16), &plan.new_text, window, cx);
        self.set_selection_after_edit(plan.selection, cx);
    }

    /// Run a per-cursor command at every cursor, as one undo step.
    fn edit_each_cursor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl Fn(&Self) -> Option<EditPlan>,
    ) {
        self.history.break_group();
        self.edit_each_selection(window, cx, |this, window, cx| {
            if let Some(plan) = f(this) {
                this.apply_edit_plan(plan, window, cx);
            }
        });
        self.history.break_group();
    }

    /// Every selection in document order, and the index of the primary one.
    fn all_selections(&self) -> (Vec<Range<usize>>, usize) {
        let all = self.selected_ranges();
        let primary: Range<usize> = self.selected_range.into();
        let ix = all.iter().position(|r| *r == primary).unwrap_or(0);
        (all, ix)
    }

    /// Apply a line command's plan over every selection as one undo step.
    fn apply_lines_plan(
        &mut self,
        plan: LinesPlan,
        primary: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range_utf16 = self.range_to_utf16(&plan.range);
        self.history.break_group();
        self.replace_text_in_range_silent(Some(range_utf16), &plan.new_text, window, cx);
        self.history.break_group();

        let mut selections = plan.selections;
        if selections.len() <= 1 {
            if let Some(selection) = selections.pop() {
                self.set_selection_after_edit(selection, cx);
            }
            return;
        }
        // `set_selected_ranges` makes the last range the primary one.
        let main = selections.remove(primary.min(selections.len() - 1));
        selections.push(main);
        self.set_selected_ranges(selections, cx);
        self.record_selections_after_edit();
        self.scroll_to(self.cursor(), None, cx);
        self.pause_blink_cursor(cx);
    }

    /// An IME commit of an ASCII bracket or quote pairs like typing it, as
    /// VS Code does: the marked text goes, and the character is typed in its
    /// place. Everything else an IME commits (CJK text, full-width
    /// punctuation) goes in untouched.
    fn handle_ime_committed_pair(
        &mut self,
        range_utf16: Option<&Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(marked) = self.ime_marked_range else {
            return false;
        };
        let pairable = new_text.len() == 1
            && new_text
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_punctuation());
        if self.silent_replace_text
            || !pairable
            || !self.mode.is_code_editor()
            || !self.mode.is_multi_line()
            || self.disabled
        {
            return false;
        }
        let marked: Range<usize> = marked.into();
        if let Some(range_utf16) = range_utf16
            && self.range_from_utf16(range_utf16) != marked
        {
            return false;
        }
        let marked_utf16 = self.range_to_utf16(&marked);
        self.replace_text_in_range_silent(Some(marked_utf16), "", window, cx);
        // The removal and what replaces it are the one keystroke.
        self.history.start_grouping();
        if !self.handle_typed_pair(None, new_text, window, cx) {
            gpui::EntityInputHandler::replace_text_in_range(self, None, new_text, window, cx);
        }
        true
    }

    /// Put the primary selection at `selection` without touching the undo
    /// history.
    fn place_selection(&mut self, selection: Range<usize>, cx: &mut Context<Self>) {
        let len = self.text.len();
        self.selected_range = (selection.start.min(len)..selection.end.min(len)).into();
        self.selection_reversed = false;
        self.selected_word_range = None;
        self.update_preferred_column();
        self.scroll_to(self.cursor(), None, cx);
        self.pause_blink_cursor(cx);
        cx.notify();
    }

    /// Put the primary selection where an edit just left it, and record that
    /// as the edit's undo step's "after".
    fn set_selection_after_edit(&mut self, selection: Range<usize>, cx: &mut Context<Self>) {
        self.place_selection(selection, cx);
        self.record_selections_after_edit();
    }

    fn selection(&self) -> Range<usize> {
        self.selected_range.into()
    }

    /// The line comment toggle over every selection, or the per-line block
    /// comment for languages without a line comment.
    fn line_comment_plan(&self, selections: &[Range<usize>]) -> Option<LinesPlan> {
        let config = self.editing_language();
        match (config.line_comment, config.block_comment) {
            (Some(token), _) => {
                toggle_line_comment(&self.text, selections, token, self.mode.tab_size())
            }
            (None, Some((open, close))) => {
                toggle_line_block_comment(&self.text, selections, open, close)
            }
            (None, None) => None,
        }
    }

    pub(super) fn toggle_line_comment(
        &mut self,
        _: &ToggleLineComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editing_commands_enabled() {
            cx.propagate();
            return;
        }
        let (selections, primary) = self.all_selections();
        if let Some(plan) = self.line_comment_plan(&selections) {
            self.apply_lines_plan(plan, primary, window, cx);
        }
    }

    pub(super) fn toggle_block_comment(
        &mut self,
        _: &ToggleBlockComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editing_commands_enabled() {
            cx.propagate();
            return;
        }
        let Some((open, close)) = self.editing_language().block_comment else {
            // No block comment in this language: fall back to line comments.
            let (selections, primary) = self.all_selections();
            if let Some(plan) = self.line_comment_plan(&selections) {
                self.apply_lines_plan(plan, primary, window, cx);
            }
            return;
        };
        self.edit_each_cursor(window, cx, |this| {
            toggle_block_comment(&this.text, &this.selection(), open, close)
        });
    }

    fn move_lines_by(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editing_commands_enabled() {
            cx.propagate();
            return;
        }
        let (selections, primary) = self.all_selections();
        if let Some(plan) = move_lines(&self.text, &selections, up) {
            self.apply_lines_plan(plan, primary, window, cx);
        }
    }

    pub(super) fn move_line_up(
        &mut self,
        _: &MoveLineUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_lines_by(true, window, cx);
    }

    pub(super) fn move_line_down(
        &mut self,
        _: &MoveLineDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_lines_by(false, window, cx);
    }

    fn copy_lines_by(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editing_commands_enabled() {
            cx.propagate();
            return;
        }
        let (selections, primary) = self.all_selections();
        if let Some(plan) = copy_lines(&self.text, &selections, up) {
            self.apply_lines_plan(plan, primary, window, cx);
        }
    }

    pub(super) fn copy_line_up(
        &mut self,
        _: &CopyLineUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy_lines_by(true, window, cx);
    }

    pub(super) fn copy_line_down(
        &mut self,
        _: &CopyLineDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy_lines_by(false, window, cx);
    }

    pub(super) fn delete_line(
        &mut self,
        _: &DeleteLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editing_commands_enabled() {
            cx.propagate();
            return;
        }
        let (selections, primary) = self.all_selections();
        // The primary caret keeps its own column; for the others, the end of
        // their range stands in for the caret.
        let heads: Vec<usize> = selections
            .iter()
            .enumerate()
            .map(|(ix, s)| if ix == primary { self.cursor() } else { s.end })
            .collect();
        let Some(plan) = delete_lines(&self.text, &selections, &heads) else {
            return;
        };
        if plan.range.is_empty() {
            return;
        }
        self.apply_lines_plan(plan, primary, window, cx);
    }

    fn insert_line_at(&mut self, above: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editing_commands_enabled() {
            cx.propagate();
            return;
        }
        let (selections, primary) = self.all_selections();
        let heads: Vec<usize> = selections
            .iter()
            .enumerate()
            .map(|(ix, s)| if ix == primary { self.cursor() } else { s.end })
            .collect();
        if let Some(plan) = insert_lines(&self.text, &heads, above) {
            self.apply_lines_plan(plan, primary, window, cx);
        }
    }

    pub(super) fn insert_line_below(
        &mut self,
        _: &InsertLineBelow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.insert_line_at(false, window, cx);
    }

    pub(super) fn insert_line_above(
        &mut self,
        _: &InsertLineAbove,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.insert_line_at(true, window, cx);
    }

    pub(super) fn select_line_action(
        &mut self,
        _: &SelectLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.is_code_editor() || !self.mode.is_multi_line() {
            cx.propagate();
            return;
        }
        let range = select_lines(&self.text, &self.selection());
        self.selected_range = range.into();
        self.selection_reversed = false;
        self.selected_word_range = None;
        self.scroll_to(self.cursor(), None, cx);
        cx.notify();
    }

    pub(super) fn move_to_matching_bracket(
        &mut self,
        _: &MoveToMatchingBracket,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.is_code_editor() {
            cx.propagate();
            return;
        }
        let cursor = self.cursor();
        let Some((open, close)) = self.bracket_pair_near(cursor) else {
            return;
        };
        // Land outside the other bracket, so pressing again jumps back.
        let target = if cursor == open || cursor == open + 1 {
            close + 1
        } else {
            open
        };
        self.move_to(target, None, cx);
    }

    fn bracket_pair_near(&self, cursor: usize) -> Option<(usize, usize)> {
        let config = self.editing_language();
        let tree_pair = self.with_syntax_tree(|tree| {
            bracket_pair_at(&self.text, cursor, &|offset| {
                tree_offset_in_string_or_comment(tree, offset)
            })
        });
        if let Some(pair) = tree_pair {
            return pair;
        }
        let text = &self.text;
        if let Some(spans) = self.lexed_spans(&config) {
            return bracket_pair_at(text, cursor, &|offset| {
                lexer::offset_in_span(&spans, offset)
            });
        }
        let skip = |offset: usize| {
            let row = text.offset_to_point(offset).row;
            let line_start = text.line_start_offset(row);
            if offset - line_start > MAX_HEURISTIC_LINE {
                return false;
            }
            let prefix = text.slice(line_start..offset).to_string();
            prefix_in_string_or_comment(&prefix, &config)
        };
        bracket_pair_at(text, cursor, &skip)
    }

    /// The byte ranges of the bracket next to each bare cursor and its
    /// match, for the element to highlight.
    pub(super) fn bracket_highlight_ranges(&self) -> Vec<Range<usize>> {
        /// Past this many cursors, only the primary one is highlighted.
        const MAX_CURSORS: usize = 64;
        if !self.mode.is_code_editor() || self.ime_marked_range.is_some() {
            return vec![];
        }
        let primary = self.selection();
        let cursors: Vec<usize> = if self.extra_selections.len() < MAX_CURSORS {
            self.selected_ranges()
                .into_iter()
                .filter(|r| r.is_empty())
                .map(|r| r.start)
                .collect()
        } else if primary.is_empty() {
            vec![primary.start]
        } else {
            vec![]
        };
        let mut ranges: Vec<Range<usize>> = cursors
            .into_iter()
            .filter_map(|cursor| self.bracket_pair_near(cursor))
            .flat_map(|(open, close)| [open..open + 1, close..close + 1])
            .collect();
        ranges.sort_by_key(|r| r.start);
        ranges.dedup();
        ranges
    }

    /// Carry the editing state through an edit of the text: `range` (before
    /// the edit) was replaced by `new_len` bytes.
    pub(super) fn on_text_edited(&mut self, range: &Range<usize>, new_len: usize) {
        if self.multi_edit.is_some() && self.editing.lexed.get_mut().is_some() {
            let before = self.editing.lexed_valid_before.get().unwrap_or(usize::MAX);
            self.editing
                .lexed_valid_before
                .set(Some(before.min(range.start)));
        } else {
            self.editing.lexed.get_mut().take();
            self.editing.lexed_valid_before.set(None);
        }
        let pairs = std::mem::take(&mut self.editing.auto_closed);
        self.editing.auto_closed = pairs
            .into_iter()
            .filter_map(|pair| map_auto_closed(pair, range, new_len))
            .collect();
    }

    /// Stop tracking the auto-inserted closers no cursor is inside any more.
    pub(super) fn prune_auto_closed(&mut self) {
        if self.editing.auto_closed.is_empty() || self.multi_edit.is_some() {
            return;
        }
        let cursors = self.selected_ranges();
        self.editing.auto_closed.retain(|pair| {
            cursors
                .iter()
                .any(|c| pair.start <= c.start && c.end <= pair.end)
        });
    }

    /// The tracked pair whose closer sits right at `cursor`.
    fn auto_closed_at(&self, cursor: usize) -> Option<usize> {
        self.editing
            .auto_closed
            .iter()
            .position(|pair| pair.end == cursor && self.text.char_at(cursor) == Some(pair.closer))
    }

    /// Handle a typed character for bracket and quote pairing.
    ///
    /// Returns true when the input was fully handled. Only plain typing
    /// reaches here: programmatic and IME edits pass through untouched.
    pub(super) fn handle_typed_pair(
        &mut self,
        range_utf16: Option<&Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.handle_ime_committed_pair(range_utf16, new_text, window, cx) {
            return true;
        }
        if self.silent_replace_text || !self.editing_commands_enabled() {
            return false;
        }
        let selection = self.selection();
        if let Some(range_utf16) = range_utf16
            && self.range_from_utf16(range_utf16) != selection
        {
            return false;
        }
        if self.multi_edit.is_none() {
            self.prune_auto_closed();
        }
        let config = self.editing_language();
        let in_string = self.cursor_in_string_or_comment(selection.start, &config);
        let tracked = selection
            .is_empty()
            .then(|| self.auto_closed_at(selection.start))
            .flatten();
        match auto_pair(
            &self.text,
            &selection,
            new_text,
            &config,
            in_string,
            tracked.is_some(),
        ) {
            Some(Typed::Edit(plan)) => {
                let inserted_pair = selection.is_empty() && plan.range.is_empty();
                let closer = plan.new_text.chars().nth(1);
                let range_utf16 = self.range_to_utf16(&plan.range);
                self.replace_text_in_range_silent(Some(range_utf16), &plan.new_text, window, cx);
                if inserted_pair && let Some(closer) = closer {
                    let inside = plan.selection.start;
                    self.editing.auto_closed.push(AutoClosedPair {
                        start: inside,
                        end: inside,
                        closer,
                    });
                }
                self.set_selection_after_edit(plan.selection, cx);
                true
            }
            Some(Typed::Skip(offset)) => {
                if let Some(ix) = tracked {
                    self.editing.auto_closed.remove(ix);
                }
                // Only the caret moves: no edit, so the undo step before
                // keeps the cursors it recorded.
                self.place_selection(offset..offset, cx);
                true
            }
            None => false,
        }
    }

    /// Backspace inside an empty pair deletes both halves.
    pub(super) fn handle_backspace_pair(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.editing_commands_enabled() || !self.selected_range.is_empty() {
            return false;
        }
        if self.multi_edit.is_none() {
            self.prune_auto_closed();
        }
        // Only a pair whose closer was typed for the user, and still empty.
        let cursor = self.cursor();
        let Some(ix) = self.auto_closed_at(cursor) else {
            return false;
        };
        if self.editing.auto_closed[ix].start != cursor {
            return false;
        }
        let Some(range) = backspace_pair(&self.text, cursor) else {
            return false;
        };
        self.editing.auto_closed.remove(ix);
        let range_utf16 = self.range_to_utf16(&range);
        self.replace_text_in_range_silent(Some(range_utf16), "", window, cx);
        true
    }

    fn transform_case_action(&mut self, case: Case, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editing_commands_enabled() {
            cx.propagate();
            return;
        }
        self.edit_each_cursor(window, cx, |this| {
            transform::case_plan(&this.text, &this.selection(), case)
        });
    }

    pub(super) fn transform_to_uppercase(
        &mut self,
        _: &TransformToUppercase,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.transform_case_action(Case::Upper, window, cx);
    }

    pub(super) fn transform_to_lowercase(
        &mut self,
        _: &TransformToLowercase,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.transform_case_action(Case::Lower, window, cx);
    }

    pub(super) fn transform_to_title_case(
        &mut self,
        _: &TransformToTitleCase,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.transform_case_action(Case::Title, window, cx);
    }

    pub(super) fn trim_trailing_whitespace_action(
        &mut self,
        _: &TrimTrailingWhitespace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editing_commands_enabled() {
            cx.propagate();
            return;
        }
        let (selections, primary) = self.all_selections();
        if let Some(plan) = transform::trim_trailing_whitespace(&self.text, &selections) {
            self.apply_lines_plan(plan, primary, window, cx);
        }
    }

    pub(super) fn join_lines_action(
        &mut self,
        _: &JoinLines,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editing_commands_enabled() {
            cx.propagate();
            return;
        }
        let (selections, primary) = self.all_selections();
        if let Some(plan) = transform::join_lines(&self.text, &selections) {
            self.apply_lines_plan(plan, primary, window, cx);
        }
    }

    pub(super) fn remove_surrounding_brackets(
        &mut self,
        _: &RemoveSurroundingBrackets,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editing_commands_enabled() {
            cx.propagate();
            return;
        }
        let config = self.editing_language();
        self.edit_each_cursor(window, cx, |this| {
            let selection = this.selection();
            if let Some(plan) = this.with_syntax_tree(|tree| {
                transform::unwrap_brackets(&this.text, &selection, &|offset| {
                    tree_offset_in_string_or_comment(tree, offset)
                })
            }) {
                return plan;
            }
            match this.lexed_spans(&config) {
                Some(spans) => transform::unwrap_brackets(&this.text, &selection, &|offset| {
                    lexer::offset_in_span(&spans, offset)
                }),
                None => transform::unwrap_brackets(&this.text, &selection, &|_| false),
            }
        });
    }

    /// Enter after an opening bracket opens an indented block.
    pub(super) fn handle_smart_newline(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.editing_commands_enabled() || !self.selected_range.is_empty() {
            return false;
        }
        let Some(plan) = smart_newline(&self.text, self.cursor(), self.mode.tab_size()) else {
            return false;
        };
        let range_utf16 = self.range_to_utf16(&plan.range);
        self.replace_text_in_range_silent(Some(range_utf16), &plan.new_text, window, cx);
        self.set_selection_after_edit(plan.selection, cx);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAB4: TabSize = TabSize {
        tab_size: 4,
        hard_tabs: false,
    };
    const TAB2: TabSize = TabSize {
        tab_size: 2,
        hard_tabs: false,
    };
    const HARD4: TabSize = TabSize {
        tab_size: 4,
        hard_tabs: true,
    };

    /// Split `text` with `|` markers into the text and a selection: one
    /// marker is a cursor, two are a selection.
    fn parse(marked: &str) -> (String, Range<usize>) {
        let marks: Vec<usize> = marked.match_indices('|').map(|(ix, _)| ix).collect();
        let text = marked.replace('|', "");
        let range = match marks.as_slice() {
            [a] => *a..*a,
            [a, b] => *a..*b - 1,
            _ => panic!("expected one or two markers in {marked:?}"),
        };
        (text, range)
    }

    /// Render text with the selection marked the way `parse` reads it.
    fn mark(text: &str, selection: &Range<usize>) -> String {
        let mut out = text.to_string();
        if selection.is_empty() {
            out.insert(selection.start, '|');
        } else {
            out.insert(selection.end, '|');
            out.insert(selection.start, '|');
        }
        out
    }

    fn run(marked: &str, f: impl Fn(&Rope, &Range<usize>) -> Option<EditPlan>) -> Option<String> {
        let (text, selection) = parse(marked);
        let rope = Rope::from(text.as_str());
        let plan = f(&rope, &selection)?;
        let after = plan.apply(&text);
        Some(mark(&after, &plan.selection))
    }

    fn run_lines(
        marked: &str,
        f: impl Fn(&Rope, &[Range<usize>]) -> Option<LinesPlan>,
    ) -> Option<String> {
        let (text, selection) = parse(marked);
        let rope = Rope::from(text.as_str());
        let plan = f(&rope, std::slice::from_ref(&selection))?;
        assert_eq!(plan.selections.len(), 1);
        let after = plan.apply(&text);
        Some(mark(&after, &plan.selections[0]))
    }

    fn comment(marked: &str) -> String {
        run_lines(marked, |t, s| toggle_line_comment(t, s, "//", TAB4)).unwrap()
    }

    #[test]
    fn test_language_config() {
        assert_eq!(language_config("rust").line_comment, Some("//"));
        assert_eq!(language_config("Rust").line_comment, Some("//"));
        for lang in [
            "javascript",
            "typescript",
            "tsx",
            "jsx",
            "go",
            "c",
            "cpp",
            "java",
        ] {
            assert_eq!(language_config(lang).line_comment, Some("//"), "{lang}");
        }
        for lang in ["kotlin", "swift", "zig", "json", "jsonc"] {
            assert_eq!(language_config(lang).line_comment, Some("//"), "{lang}");
        }
        for lang in [
            "python", "ruby", "bash", "sh", "zsh", "fish", "toml", "yaml",
        ] {
            assert_eq!(language_config(lang).line_comment, Some("#"), "{lang}");
        }
        assert_eq!(language_config("sql").line_comment, Some("--"));
        assert_eq!(language_config("lua").line_comment, Some("--"));
        assert_eq!(language_config("css").line_comment, None);
        assert_eq!(language_config("css").block_comment, Some(("/*", "*/")));
        for lang in ["html", "xml", "markdown"] {
            let config = language_config(lang);
            assert_eq!(config.line_comment, None, "{lang}");
            assert_eq!(config.block_comment, Some(("<!--", "-->")), "{lang}");
        }
        assert_eq!(language_config("text"), LanguageConfig::PLAIN);
    }

    #[test]
    fn test_comment_single_line() {
        assert_eq!(comment("fn |main() {}"), "// fn |main() {}");
        assert_eq!(comment("    let |x = 1;"), "    // let |x = 1;");
        assert_eq!(comment("    // let |x = 1;"), "    let |x = 1;");
        // No space after the token: removes the token alone.
        assert_eq!(comment("//let |x = 1;"), "let |x = 1;");
        // Cursor before the insertion point stays put.
        assert_eq!(comment("|    x"), "|    // x");
    }

    #[test]
    fn test_comment_multi_line_aligns_at_min_indent() {
        assert_eq!(
            comment("|    a\n        b\n    c|"),
            "|    // a\n    //     b\n    // c|"
        );
        // Deeper first line, shallower second line.
        assert_eq!(comment("        |a\n    b|"), "    //     |a\n    // b|");
    }

    #[test]
    fn test_comment_skips_blank_lines() {
        assert_eq!(comment("|a\n\n   \nb|"), "|// a\n\n   \n// b|");
        // Blank lines do not count toward the minimum indent.
        assert_eq!(comment("|    a\n\n    b|"), "|    // a\n\n    // b|");
    }

    #[test]
    fn test_comment_blank_line_only() {
        assert_eq!(comment("a\n    |\nb"), "a\n    // |\nb");
        assert_eq!(comment("|"), "// |");
    }

    #[test]
    fn test_uncomment_when_all_commented() {
        assert_eq!(comment("|// a\n\n    // b\n//c|"), "|a\n\n    b\nc|");
    }

    #[test]
    fn test_comment_when_partly_commented() {
        // Mixed: comment everything again (nested), like VS Code.
        assert_eq!(comment("|// a\nb|"), "|// // a\n// b|");
    }

    #[test]
    fn test_comment_mixed_tabs_and_spaces() {
        // Tab = 4 columns, so both lines sit at column 4.
        assert_eq!(comment("|\ta\n    b|"), "|\t// a\n    // b|");
        // With 2-column tabs the tab line is shallower.
        let out = run_lines("|\ta\n    b|", |t, s| toggle_line_comment(t, s, "//", TAB2)).unwrap();
        assert_eq!(out, "|\t// a\n  //   b|");
        // A tab that straddles the minimum column: insert before it.
        assert_eq!(comment("|  \ta\n  b|"), "|  // \ta\n  // b|");
        let hard = run_lines("|\t\ta\n\tb|", |t, s| toggle_line_comment(t, s, "#", HARD4)).unwrap();
        assert_eq!(hard, "|\t# \ta\n\t# b|");
    }

    #[test]
    fn test_comment_selection_ending_at_line_start_excludes_that_line() {
        assert_eq!(comment("|a\nb\n|c"), "|// a\n// b\n|c");
    }

    #[test]
    fn test_comment_keeps_crlf_line_endings() {
        assert_eq!(comment("|a\r\nb|"), "|// a\r\n// b|");
        assert_eq!(comment("|// a\r\n// b|"), "|a\r\nb|");
    }

    #[test]
    fn test_comment_hash_token() {
        let out = run_lines("  |x = 1", |t, s| toggle_line_comment(t, s, "#", TAB4)).unwrap();
        assert_eq!(out, "  # |x = 1");
        let out = run_lines("  # |x = 1", |t, s| toggle_line_comment(t, s, "#", TAB4)).unwrap();
        assert_eq!(out, "  |x = 1");
    }

    #[test]
    fn test_comment_with_non_ascii_indent_does_not_panic() {
        // U+3000 is whitespace to `trim`, but not an indent the edits
        // measure: deciding "already commented" by `trim` sliced mid-char.
        let line = |m: &str| run_lines(m, |t, s| toggle_line_comment(t, s, "//", TAB4)).unwrap();
        assert_eq!(line("\u{3000}// |x"), "// \u{3000}// |x");
        let css =
            |m: &str| run_lines(m, |t, s| toggle_line_block_comment(t, s, "/*", "*/")).unwrap();
        assert_eq!(css("\u{3000}/* x */|"), "/* \u{3000}/* x */ */|");
    }

    #[test]
    fn test_line_block_comment() {
        let html =
            |m: &str| run_lines(m, |t, s| toggle_line_block_comment(t, s, "<!--", "-->")).unwrap();
        assert_eq!(html("  <p>|hi</p>"), "  <!-- <p>|hi</p> -->");
        assert_eq!(html("  <!-- <p>|hi</p> -->"), "  <p>|hi</p>");
        assert_eq!(html("|a\n  b|"), "|<!-- a -->\n  <!-- b -->|");
        assert_eq!(html("|<!-- a -->\n  <!-- b -->|"), "|a\n  b|");
        let css =
            |m: &str| run_lines(m, |t, s| toggle_line_block_comment(t, s, "/*", "*/")).unwrap();
        assert_eq!(css("a { color: red; }|  "), "/* a { color: red; } */|  ");
        assert_eq!(css("/*x*/|"), "x|");
        // Blank line: an empty comment with the cursor inside.
        assert_eq!(css("|"), "/* | */");
    }

    #[test]
    fn test_block_comment() {
        let block = |m: &str| run(m, |t, s| toggle_block_comment(t, s, "/*", "*/")).unwrap();
        assert_eq!(block("let x = |1 + 2|;"), "let x = /* |1 + 2| */;");
        assert_eq!(block("let x = |/* 1 + 2 */|;"), "let x = |1 + 2|;");
        // Empty selection works on the trimmed line.
        assert_eq!(block("    foo(|);"), "    /* foo(|); */");
        assert_eq!(block("    /* foo(|); */"), "    foo(|);");
    }

    fn move_up(m: &str) -> Option<String> {
        run_lines(m, |t, s| move_lines(t, s, true))
    }

    fn move_down(m: &str) -> Option<String> {
        run_lines(m, |t, s| move_lines(t, s, false))
    }

    #[test]
    fn test_move_line() {
        assert_eq!(move_up("a\nb|b\nc").unwrap(), "b|b\na\nc");
        assert_eq!(move_down("a\nb|b\nc").unwrap(), "a\nc\nb|b");
        // At the boundaries nothing happens.
        assert_eq!(move_up("a|a\nb"), None);
        assert_eq!(move_down("a\nb|b"), None);
        assert_eq!(move_up("|"), None);
        assert_eq!(move_down("|"), None);
    }

    #[test]
    fn test_move_lines_with_selection() {
        assert_eq!(move_up("x\n|a\nb|\ny").unwrap(), "|a\nb|\nx\ny");
        assert_eq!(move_down("x\n|a\nb|\ny").unwrap(), "x\ny\n|a\nb|");
        // A selection ending at the start of a line leaves that line alone.
        assert_eq!(move_down("|a\n|b\nc").unwrap(), "b\n|a\n|c");
    }

    #[test]
    fn test_move_line_past_trailing_newline() {
        // The empty last line is a line like any other.
        assert_eq!(move_down("a\nb|\n").unwrap(), "a\n\nb|");
        assert_eq!(move_up("a\n|").unwrap(), "|\na");
    }

    #[test]
    fn test_copy_lines() {
        let down = |m: &str| run_lines(m, |t, s| copy_lines(t, s, false)).unwrap();
        let up = |m: &str| run_lines(m, |t, s| copy_lines(t, s, true)).unwrap();
        assert_eq!(down("a|b\nc"), "ab\na|b\nc");
        assert_eq!(up("a|b\nc"), "a|b\nab\nc");
        assert_eq!(down("x\n|a\nb|"), "x\na\nb\n|a\nb|");
        assert_eq!(up("x\n|a\nb|"), "x\n|a\nb|\na\nb");
        assert_eq!(down("|"), "\n|");
    }

    #[test]
    fn test_delete_lines() {
        let delete = |m: &str| run_lines(m, |t, s| delete_lines(t, s, &[s[0].end])).unwrap();
        assert_eq!(delete("aaa\nb|b\ncccc"), "aaa\nc|ccc");
        // Column clamps to a shorter next line.
        assert_eq!(delete("aaa\nbbbb|b\nc"), "aaa\nc|");
        // Last line: the cursor goes up.
        assert_eq!(delete("aaaa\nb|b"), "a|aaa");
        // Only line.
        assert_eq!(delete("a|bc"), "|");
        assert_eq!(delete("|a\nb|\nc"), "c|");
        // Multi-byte column.
        assert_eq!(delete("中文|x\n中文字"), "中文|字");
    }

    #[test]
    fn test_insert_line() {
        let below = |m: &str| run(m, |t, s| Some(insert_line(t, s.start, false))).unwrap();
        let above = |m: &str| run(m, |t, s| Some(insert_line(t, s.start, true))).unwrap();
        assert_eq!(below("    fo|o\nbar"), "    foo\n    |\nbar");
        assert_eq!(above("    fo|o\nbar"), "    |\n    foo\nbar");
        assert_eq!(below("\tx|"), "\tx\n\t|");
        assert_eq!(below("a|\r\nb"), "a\r\n|\r\nb");
    }

    #[test]
    fn test_select_lines() {
        let select = |m: &str| {
            let (text, s) = parse(m);
            let range = select_lines(&Rope::from(text.as_str()), &s);
            mark(&text, &range)
        };
        assert_eq!(select("a\nb|b\nc"), "a\n|bb\n|c");
        assert_eq!(select("a\n|bb\n|c"), "a\n|bb\nc|");
        assert_eq!(select("a\nb|b"), "a\n|bb|");
        assert_eq!(select("a|a\nb"), "|aa\n|b");
    }

    fn typed(m: &str, c: &str, lang: &str) -> Option<String> {
        typed_with(m, c, lang, false)
    }

    /// `typed`, with the character after the cursor an auto-inserted closer.
    fn typed_over(m: &str, c: &str, lang: &str) -> Option<String> {
        typed_with(m, c, lang, true)
    }

    fn typed_with(m: &str, c: &str, lang: &str, overtype: bool) -> Option<String> {
        let (text, selection) = parse(m);
        let rope = Rope::from(text.as_str());
        let config = language_config(lang);
        let row = rope.offset_to_point(selection.start).row;
        let prefix = rope
            .slice(rope.line_start_offset(row)..selection.start)
            .to_string();
        let in_string = prefix_in_string_or_comment(&prefix, &config);
        match auto_pair(&rope, &selection, c, &config, in_string, overtype)? {
            Typed::Edit(plan) => Some(mark(&plan.apply(&text), &plan.selection)),
            Typed::Skip(offset) => Some(mark(&text, &(offset..offset))),
        }
    }

    #[test]
    fn test_auto_pair_brackets() {
        assert_eq!(typed("foo|", "(", "rust").unwrap(), "foo(|)");
        assert_eq!(typed("|", "[", "rust").unwrap(), "[|]");
        assert_eq!(typed("x = |;", "{", "rust").unwrap(), "x = {|};");
        assert_eq!(typed("f(|)", "(", "rust").unwrap(), "f((|))");
        // Not in front of a word.
        assert_eq!(typed("|foo", "(", "rust"), None);
        // Brackets pair in plain text too.
        assert_eq!(typed("see |", "(", "text").unwrap(), "see (|)");
        // Multi-character input is never paired.
        assert_eq!(typed("|", "()", "rust"), None);
    }

    #[test]
    fn test_auto_pair_overtype() {
        assert_eq!(typed_over("foo(|)", ")", "rust").unwrap(), "foo()|");
        assert_eq!(typed_over("[1, 2|]", "]", "rust").unwrap(), "[1, 2]|");
        assert_eq!(typed_over("\"abc|\"", "\"", "rust").unwrap(), "\"abc\"|");
        // A closer the user typed is not stepped over.
        assert_eq!(typed("foo(|)", ")", "rust"), None);
        // A closer with nothing to step over is a plain insert.
        assert_eq!(typed_over("foo|", ")", "rust"), None);
        // A quote in front of a string opens a new one instead of skipping.
        assert_eq!(typed("x = |\"a\"", "\"", "rust"), None);
    }

    #[test]
    fn test_auto_pair_quotes() {
        assert_eq!(typed("x = |", "\"", "rust").unwrap(), "x = \"|\"");
        assert_eq!(typed("x = |", "'", "python").unwrap(), "x = '|'");
        assert_eq!(typed("x = |", "`", "typescript").unwrap(), "x = `|`");
        // Rust: `'` is a lifetime, never paired.
        assert_eq!(typed("fn f<|>", "'", "rust"), None);
        assert_eq!(typed("&|", "'", "rust"), None);
        // After a word character it is an apostrophe.
        assert_eq!(typed("don|", "'", "python"), None);
        assert_eq!(typed("it|", "'", "text"), None);
        assert_eq!(typed("say |", "\"", "text"), None);
        // Markdown pairs backticks only.
        assert_eq!(typed("use |", "`", "markdown").unwrap(), "use `|`");
        assert_eq!(typed("say |", "\"", "markdown"), None);
        // Not inside a string or comment.
        assert_eq!(typed("x = \"a |", "'", "python"), None);
        assert_eq!(typed("// it |", "\"", "rust"), None);
        assert_eq!(typed("# say |", "'", "python"), None);
    }

    #[test]
    fn test_auto_pair_surround() {
        assert_eq!(
            typed("x = |a + b|;", "(", "rust").unwrap(),
            "x = (|a + b|);"
        );
        assert_eq!(typed("|word|", "\"", "rust").unwrap(), "\"|word|\"");
        assert_eq!(typed("|word|", "`", "markdown").unwrap(), "`|word|`");
        assert_eq!(typed("|a\nb|", "{", "rust").unwrap(), "{|a\nb|}");
        // A quote the language does not pair replaces the selection.
        assert_eq!(typed("|word|", "'", "rust"), None);
        // A closer typed over a selection is a plain replace.
        assert_eq!(typed("|word|", ")", "rust"), None);
    }

    #[test]
    fn test_backspace_pair() {
        let bs = |m: &str| {
            let (text, s) = parse(m);
            backspace_pair(&Rope::from(text.as_str()), s.start)
        };
        assert_eq!(bs("f(|)"), Some(1..3));
        assert_eq!(bs("[|]"), Some(0..2));
        assert_eq!(bs("{|}"), Some(0..2));
        assert_eq!(bs("\"|\""), Some(0..2));
        assert_eq!(bs("'|'"), Some(0..2));
        assert_eq!(bs("(|]"), None);
        assert_eq!(bs("(a|)"), None);
        assert_eq!(bs("|()"), None);
        assert_eq!(bs("()|"), None);
    }

    #[test]
    fn test_smart_newline() {
        let enter = |m: &str, tab: TabSize| run(m, |t, s| smart_newline(t, s.start, tab));
        assert_eq!(enter("fn a() {|}", TAB4).unwrap(), "fn a() {\n    |\n}");
        assert_eq!(
            enter("    if x {|}", TAB4).unwrap(),
            "    if x {\n        |\n    }"
        );
        assert_eq!(enter("\tf(|)", HARD4).unwrap(), "\tf(\n\t\t|\n\t)");
        // Whitespace between the pair goes away.
        assert_eq!(enter("x = [ | ]", TAB2).unwrap(), "x = [\n  |\n]");
        // Opener at the end of the line: indent one level.
        assert_eq!(enter("  foo {|", TAB2).unwrap(), "  foo {\n    |");
        assert_eq!(enter("  foo { |  ", TAB2).unwrap(), "  foo {\n    |");
        // Not after an opener, or text follows: fall back to plain Enter.
        assert_eq!(enter("foo|", TAB4), None);
        assert_eq!(enter("foo(|bar)", TAB4), None);
        assert_eq!(enter("{}|", TAB4), None);
    }

    #[test]
    fn test_prefix_in_string_or_comment() {
        let rust = language_config("rust");
        let py = language_config("python");
        assert!(!prefix_in_string_or_comment("let x = ", &rust));
        assert!(prefix_in_string_or_comment("let x = \"abc", &rust));
        assert!(!prefix_in_string_or_comment("let x = \"abc\"", &rust));
        assert!(prefix_in_string_or_comment("let x = \"a\\\"b", &rust));
        assert!(prefix_in_string_or_comment("x // note", &rust));
        assert!(!prefix_in_string_or_comment("\"//\" + ", &rust));
        assert!(prefix_in_string_or_comment("x /* note", &rust));
        assert!(!prefix_in_string_or_comment("x /* note */ y", &rust));
        // Lifetimes do not open strings in Rust.
        assert!(!prefix_in_string_or_comment("fn f<'a>(x: &'a ", &rust));
        assert!(prefix_in_string_or_comment("x = 'ab", &py));
        assert!(prefix_in_string_or_comment("x = 1  # ", &py));
    }

    fn pair(m: &str) -> Option<(usize, usize)> {
        let (text, s) = parse(m);
        bracket_pair_at(&Rope::from(text.as_str()), s.start, &|_| false)
    }

    #[test]
    fn test_bracket_pair() {
        assert_eq!(pair("|(a)"), Some((0, 2)));
        assert_eq!(pair("(a)|"), Some((0, 2)));
        assert_eq!(pair("(|a)"), Some((0, 2)));
        assert_eq!(pair("(a|)"), Some((0, 2)));
        assert_eq!(pair("a|b"), None);
        assert_eq!(pair("|"), None);
        // Unbalanced.
        assert_eq!(pair("|(a"), None);
        assert_eq!(pair("a)|"), None);
    }

    #[test]
    fn test_bracket_pair_nesting() {
        //                   0123456789
        assert_eq!(pair("|(a(b)c)"), Some((0, 6)));
        assert_eq!(pair("(a|(b)c)"), Some((2, 4)));
        assert_eq!(pair("(a(b)|c)"), Some((2, 4)));
        assert_eq!(pair("(a(b)c)|"), Some((0, 6)));
        // Other bracket kinds do not count toward the depth.
        assert_eq!(pair("|{ [ ( ] }"), Some((0, 8)));
        // A closer before the cursor wins over an opener after it.
        assert_eq!(pair("(a)|(b)"), Some((0, 2)));
        // Multi-line and multi-byte.
        assert_eq!(pair("|{\n  中文\n}"), Some((0, 11)));
    }

    #[test]
    fn test_bracket_pair_skips_strings() {
        let text = "f(\")\", ')') + g(x)";
        let rope = Rope::from(text);
        let config = language_config("python");
        let skip = |offset: usize| prefix_in_string_or_comment(&text[..offset], &config);
        assert_eq!(bracket_pair_at(&rope, 1, &skip), Some((1, 10)));
        assert_eq!(bracket_pair_at(&rope, 11, &skip), Some((1, 10)));
        // A bracket inside a string does not match at all.
        assert_eq!(bracket_pair_at(&rope, 3, &skip), None);
    }

    fn parse_json(text: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_json::LANGUAGE.into())
            .unwrap();
        parser.parse(text, None).unwrap()
    }

    #[test]
    fn test_bracket_pair_with_syntax_tree() {
        let text = r#"{"a": "}]", "b": [1, "["]}"#;
        let tree = parse_json(text);
        let rope = Rope::from(text);
        let skip = |offset: usize| tree_offset_in_string_or_comment(&tree, offset);
        let last = text.len() - 1;
        assert_eq!(bracket_pair_at(&rope, 0, &skip), Some((0, last)));
        assert_eq!(bracket_pair_at(&rope, text.len(), &skip), Some((0, last)));
        let open = text.find('[').unwrap();
        let close = text.rfind(']').unwrap();
        assert_eq!(bracket_pair_at(&rope, open, &skip), Some((open, close)));
        // The `}` inside `"}]"` is in a string.
        assert_eq!(bracket_pair_at(&rope, 7, &skip), None);
    }

    #[test]
    fn test_tree_cursor_in_string() {
        let text = r#"{"a": "xy", "b": 1}"#;
        let tree = parse_json(text);
        let value = text.find("\"xy\"").unwrap();
        assert!(!tree_cursor_in_string_or_comment(&tree, value));
        assert!(tree_cursor_in_string_or_comment(&tree, value + 1));
        assert!(tree_cursor_in_string_or_comment(&tree, value + 3));
        assert!(!tree_cursor_in_string_or_comment(&tree, value + 4));
        assert!(!tree_cursor_in_string_or_comment(&tree, 0));
    }

    /// Run a line command over several selections given as byte ranges.
    fn multi(
        text: &str,
        selections: &[Range<usize>],
        f: impl Fn(&Rope, &[Range<usize>]) -> Option<LinesPlan>,
    ) -> Option<(String, Vec<Range<usize>>)> {
        let plan = f(&Rope::from(text), selections)?;
        Some((plan.apply(text), plan.selections))
    }

    #[test]
    fn test_multi_cursor_comment_dedupes_lines() {
        // Two cursors on line 0, one on line 2: each line commented once.
        let (out, sels) = multi("ab\ncd\nef", &[0..0, 1..1, 7..7], |t, s| {
            toggle_line_comment(t, s, "//", TAB4)
        })
        .unwrap();
        assert_eq!(out, "// ab\ncd\n// ef");
        assert_eq!(sels, vec![3..3, 4..4, 13..13]);
        // And back: all touched lines are commented, so they all uncomment.
        let (out, _) = multi("// ab\ncd\n// ef", &[3..3, 13..13], |t, s| {
            toggle_line_comment(t, s, "//", TAB4)
        })
        .unwrap();
        assert_eq!(out, "ab\ncd\nef");
        // One uncommented line among them: comment everything.
        let (out, _) = multi("// ab\ncd\nef", &[0..0, 9..9], |t, s| {
            toggle_line_comment(t, s, "//", TAB4)
        })
        .unwrap();
        assert_eq!(out, "// // ab\ncd\n// ef");
    }

    #[test]
    fn test_multi_cursor_move_lines() {
        // Cursors on neighbouring lines move as one block.
        let (out, sels) =
            multi("a\nb\nc\nd", &[2..2, 4..4], |t, s| move_lines(t, s, true)).unwrap();
        assert_eq!(out, "b\nc\na\nd");
        assert_eq!(sels, vec![0..0, 2..2]);
        // Two cursors on one line move it once.
        let (out, sels) = multi("a\nbb\nc", &[2..2, 3..3], |t, s| move_lines(t, s, false)).unwrap();
        assert_eq!(out, "a\nc\nbb");
        assert_eq!(sels, vec![4..4, 5..5]);
        // Separate blocks each swap with their neighbour.
        let (out, _) = multi("a\nb\nc\nd\ne", &[2..2, 6..6], |t, s| {
            move_lines(t, s, true)
        })
        .unwrap();
        assert_eq!(out, "b\na\nd\nc\ne");
        // A block already at the top stops the whole move.
        assert_eq!(
            multi("a\nb\nc", &[0..0, 4..4], |t, s| move_lines(t, s, true)),
            None
        );
    }

    #[test]
    fn test_multi_cursor_copy_and_delete_lines() {
        let (out, sels) = multi("a\nb\nc", &[0..0, 4..4], |t, s| copy_lines(t, s, false)).unwrap();
        assert_eq!(out, "a\na\nb\nc\nc");
        assert_eq!(sels, vec![2..2, 8..8]);
        let (out, sels) = multi("a\nb\nc", &[0..0, 4..4], |t, s| copy_lines(t, s, true)).unwrap();
        assert_eq!(out, "a\na\nb\nc\nc");
        assert_eq!(sels, vec![0..0, 6..6]);

        let (out, sels) = multi("aa\nbb\ncc\ndd", &[1..1, 7..7], |t, s| {
            delete_lines(t, s, &[1, 7])
        })
        .unwrap();
        assert_eq!(out, "bb\ndd");
        assert_eq!(sels, vec![1..1, 4..4]);
        // Two cursors on one line delete it once.
        let (out, _) = multi("aa\nbb\ncc", &[3..3, 4..4], |t, s| {
            delete_lines(t, s, &[3, 4])
        })
        .unwrap();
        assert_eq!(out, "aa\ncc");
        // The last line goes with its preceding newline.
        let (out, sels) = multi("aa\nbb\ncc", &[0..0, 7..7], |t, s| {
            delete_lines(t, s, &[0, 7])
        })
        .unwrap();
        assert_eq!(out, "bb");
        assert_eq!(sels, vec![0..0, 1..1]);
    }

    #[test]
    fn test_map_auto_closed() {
        // `(|)`: inside starts and ends at 1, the closer at 1.
        let pair = AutoClosedPair {
            start: 1,
            end: 1,
            closer: ')',
        };
        let map = |range: Range<usize>, len: usize| map_auto_closed(pair, &range, len);
        // Typing inside stretches it.
        assert_eq!(map(1..1, 3).map(|p| (p.start, p.end)), Some((1, 4)));
        // Typing in front of the opener shifts it.
        assert_eq!(map(0..0, 2).map(|p| (p.start, p.end)), Some((3, 3)));
        // Typing after the closer leaves it alone.
        assert_eq!(map(2..2, 1), Some(pair));
        // Deleting the opener or the closer ends it.
        assert_eq!(map(0..1, 0), None);
        assert_eq!(map(1..2, 0), None);
        // A deletion inside a wider pair shrinks it.
        let wide = AutoClosedPair {
            start: 1,
            end: 4,
            closer: ')',
        };
        assert_eq!(
            map_auto_closed(wide, &(2..4), 0).map(|p| (p.start, p.end)),
            Some((1, 2))
        );
    }

    #[test]
    fn test_insert_lines_dedupes_per_line() {
        let text = Rope::from("  ab\ncd");
        let plan = insert_lines(&text, &[2, 3, 6], false).unwrap();
        let out = plan.apply("  ab\ncd");
        assert_eq!(out, "  ab\n  \ncd\n");
        assert_eq!(plan.selections, vec![7..7, 7..7, 11..11]);
        let plan = insert_lines(&text, &[2, 3], true).unwrap();
        assert_eq!(plan.apply("  ab\ncd"), "  \n  ab\ncd");
        assert_eq!(plan.selections, vec![2..2, 2..2]);
    }

    #[test]
    fn test_map_offset() {
        let edits = vec![(2..2, "xx".to_string()), (5..7, String::new())];
        assert_eq!(map_offset(&edits, 0, true), 0);
        assert_eq!(map_offset(&edits, 2, true), 4);
        assert_eq!(map_offset(&edits, 2, false), 2);
        assert_eq!(map_offset(&edits, 5, true), 7);
        assert_eq!(map_offset(&edits, 6, true), 7);
        assert_eq!(map_offset(&edits, 7, true), 7);
        assert_eq!(map_offset(&edits, 9, true), 9);
    }
}

#[cfg(test)]
mod gpui_tests {
    use gpui::{
        AppContext as _, Entity, EntityInputHandler as _, Focusable as _, IntoElement,
        ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext, div,
    };

    use crate::Root;
    use crate::input::{Input, InputState, Undo};
    use crate::theme::Theme;

    struct Host {
        input: Entity<InputState>,
    }

    impl Render for Host {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            div().size_full().child(Input::new(&self.input))
        }
    }

    fn secondary(key: &str) -> String {
        if cfg!(target_os = "macos") {
            format!("cmd-{key}")
        } else {
            format!("ctrl-{key}")
        }
    }

    fn setup(
        cx: &mut TestAppContext,
        build: impl FnOnce(InputState) -> InputState + 'static,
    ) -> (Entity<InputState>, VisualTestContext) {
        let mut input = None;
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                cx.set_global(Theme::default());
                crate::input::init(cx);
                let state = cx.new(|cx| build(InputState::new(window, cx)));
                input = Some(state.clone());
                let host = cx.new(|_| Host { input: state });
                cx.new(|cx| Root::new(host, window, cx))
            })
            .unwrap()
        });
        let input = input.unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.update(|window, cx| {
            let handle = input.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.run_until_parked();
        (input, cx)
    }

    fn set_text(input: &Entity<InputState>, cx: &mut VisualTestContext, text: &str, cursor: usize) {
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value(text, window, cx);
                state.selected_range = (cursor..cursor).into();
            });
        });
        cx.run_until_parked();
    }

    fn value(input: &Entity<InputState>, cx: &mut VisualTestContext) -> String {
        cx.update(|_, cx| input.read(cx).value().to_string())
    }

    fn cursor(input: &Entity<InputState>, cx: &mut VisualTestContext) -> usize {
        cx.update(|_, cx| input.read(cx).cursor())
    }

    fn undo(input: &Entity<InputState>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| input.update(cx, |state, cx| state.undo(&Undo, window, cx)));
    }

    #[gpui::test]
    fn test_comment_keystroke_is_one_undo_step(cx: &mut TestAppContext) {
        let (input, mut cx) = setup(cx, |s| s.code_editor("rust"));
        set_text(&input, &mut cx, "fn a() {\n    b();\n}", 13);

        cx.simulate_keystrokes(&secondary("/"));
        assert_eq!(value(&input, &mut cx), "fn a() {\n    // b();\n}");

        // Typing right after the command is a separate undo step.
        cx.simulate_input("x");
        undo(&input, &mut cx);
        assert_eq!(value(&input, &mut cx), "fn a() {\n    // b();\n}");
        undo(&input, &mut cx);
        assert_eq!(value(&input, &mut cx), "fn a() {\n    b();\n}");
    }

    #[gpui::test]
    fn test_line_keystrokes(cx: &mut TestAppContext) {
        let (input, mut cx) = setup(cx, |s| s.code_editor("rust"));
        set_text(&input, &mut cx, "one\ntwo\nthree", 5);

        cx.simulate_keystrokes("alt-up");
        assert_eq!(value(&input, &mut cx), "two\none\nthree");
        assert_eq!(cursor(&input, &mut cx), 1);
        undo(&input, &mut cx);
        assert_eq!(value(&input, &mut cx), "one\ntwo\nthree");

        set_text(&input, &mut cx, "one\ntwo\nthree", 5);
        cx.simulate_keystrokes("alt-shift-down");
        assert_eq!(value(&input, &mut cx), "one\ntwo\ntwo\nthree");
        assert_eq!(cursor(&input, &mut cx), 9);

        cx.simulate_keystrokes(&secondary("shift-k"));
        assert_eq!(value(&input, &mut cx), "one\ntwo\nthree");

        set_text(&input, &mut cx, "  one\ntwo", 3);
        cx.simulate_keystrokes(&secondary("enter"));
        assert_eq!(value(&input, &mut cx), "  one\n  \ntwo");
        assert_eq!(cursor(&input, &mut cx), 8);

        cx.simulate_keystrokes(&secondary("l"));
        let selected = cx.update(|_, cx| input.read(cx).selected_range());
        assert_eq!(selected, 6..9);
    }

    #[gpui::test]
    fn test_typing_pairs(cx: &mut TestAppContext) {
        let (input, mut cx) = setup(cx, |s| s.code_editor("rust"));
        set_text(&input, &mut cx, "", 0);

        cx.simulate_input("f(");
        assert_eq!(value(&input, &mut cx), "f()");
        assert_eq!(cursor(&input, &mut cx), 2);
        cx.simulate_input(")");
        assert_eq!(value(&input, &mut cx), "f()");
        assert_eq!(cursor(&input, &mut cx), 3);

        set_text(&input, &mut cx, "x = ", 4);
        cx.simulate_input("\"");
        assert_eq!(value(&input, &mut cx), "x = \"\"");
        cx.simulate_keystrokes("backspace");
        assert_eq!(value(&input, &mut cx), "x = ");

        set_text(&input, &mut cx, "fn a() {}", 8);
        cx.simulate_keystrokes("enter");
        assert_eq!(value(&input, &mut cx), "fn a() {\n  \n}");
        assert_eq!(cursor(&input, &mut cx), 11);
    }

    /// The marked text is left alone while the IME composes; an ASCII
    /// bracket it commits pairs like a typed one, as in VS Code.
    #[gpui::test]
    fn test_ime_composition_pairs_only_on_commit(cx: &mut TestAppContext) {
        let (input, mut cx) = setup(cx, |s| s.code_editor("rust"));
        set_text(&input, &mut cx, "", 0);
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_and_mark_text_in_range(None, "(", None, window, cx);
            });
        });
        assert_eq!(value(&input, &mut cx), "(");
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "(", window, cx);
            });
        });
        assert_eq!(value(&input, &mut cx), "()");
        assert_eq!(cursor(&input, &mut cx), 1);
    }

    #[gpui::test]
    fn test_plain_multi_line_input_is_untouched(cx: &mut TestAppContext) {
        let (input, mut cx) = setup(cx, |s| s.multi_line(true));
        set_text(&input, &mut cx, "a", 1);

        cx.simulate_input("(");
        assert_eq!(value(&input, &mut cx), "a(");
        cx.simulate_keystrokes(&secondary("/"));
        assert_eq!(value(&input, &mut cx), "a(");
        cx.simulate_keystrokes("alt-up");
        assert_eq!(value(&input, &mut cx), "a(");
    }

    #[gpui::test]
    fn test_multi_cursor_commands(cx: &mut TestAppContext) {
        let (input, mut cx) = setup(cx, |s| s.code_editor("rust"));
        set_text(&input, &mut cx, "a\nb\nc", 0);
        let set = |cx: &mut VisualTestContext, ranges: Vec<std::ops::Range<usize>>| {
            cx.update(|_, cx| {
                input.update(cx, |state, cx| state.set_selected_ranges(ranges, cx));
            });
        };
        let ranges =
            |cx: &mut VisualTestContext| cx.update(|_, cx| input.read(cx).selected_ranges());

        set(&mut cx, vec![0..0, 4..4]);
        cx.simulate_keystrokes(&secondary("/"));
        assert_eq!(value(&input, &mut cx), "// a\nb\n// c");
        assert_eq!(ranges(&mut cx), vec![3..3, 10..10]);
        undo(&input, &mut cx);
        assert_eq!(value(&input, &mut cx), "a\nb\nc");

        set(&mut cx, vec![0..0, 2..2]);
        cx.simulate_keystrokes("alt-down");
        assert_eq!(value(&input, &mut cx), "c\na\nb");
        assert_eq!(ranges(&mut cx), vec![2..2, 4..4]);

        // Typing an opener pairs at every cursor.
        set_text(&input, &mut cx, "f\ng", 0);
        set(&mut cx, vec![1..1, 3..3]);
        cx.simulate_input("(");
        assert_eq!(value(&input, &mut cx), "f()\ng()");
        assert_eq!(ranges(&mut cx), vec![2..2, 6..6]);
        cx.simulate_keystrokes("backspace");
        assert_eq!(value(&input, &mut cx), "f\ng");
    }

    #[gpui::test]
    fn test_only_auto_inserted_closers_are_special(cx: &mut TestAppContext) {
        let (input, mut cx) = setup(cx, |s| s.code_editor("rust"));
        // A closer the user typed is not stepped over, nor deleted with its
        // opener.
        set_text(&input, &mut cx, "f()", 2);
        cx.simulate_input(")");
        assert_eq!(value(&input, &mut cx), "f())");
        set_text(&input, &mut cx, "f()", 2);
        cx.simulate_keystrokes("backspace");
        assert_eq!(value(&input, &mut cx), "f)");

        // Nested auto-inserted closers are stepped over one by one.
        set_text(&input, &mut cx, "", 0);
        cx.simulate_input("f((x");
        assert_eq!(value(&input, &mut cx), "f((x))");
        cx.simulate_input("))");
        assert_eq!(value(&input, &mut cx), "f((x))");
        assert_eq!(cursor(&input, &mut cx), 6);

        // Once the caret has left the pair, its closer is an ordinary one.
        set_text(&input, &mut cx, "", 0);
        cx.simulate_input("g(");
        cx.simulate_keystrokes("left left");
        cx.run_until_parked();
        cx.simulate_keystrokes("right right");
        cx.run_until_parked();
        cx.simulate_input(")");
        assert_eq!(value(&input, &mut cx), "g())");
    }

    #[gpui::test]
    fn test_bracket_highlight_at_every_cursor(cx: &mut TestAppContext) {
        let (input, mut cx) = setup(cx, |s| s.code_editor("rust"));
        // No syntax tree here: the lexer keeps the `(` in the string out.
        set_text(&input, &mut cx, "a(\"(\")\nb[1]", 0);
        cx.update(|_, cx| {
            input.update(cx, |state, cx| state.set_selected_ranges([1..1, 9..9], cx));
        });
        let ranges = cx.update(|_, cx| input.read(cx).bracket_highlight_ranges());
        assert_eq!(ranges, vec![1..2, 5..6, 8..9, 10..11]);
    }

    #[gpui::test]
    fn test_text_commands(cx: &mut TestAppContext) {
        let (input, mut cx) = setup(cx, |s| s.code_editor("rust"));
        set_text(&input, &mut cx, "let foo = 1;  \nbar", 5);
        cx.dispatch_action(super::TransformToUppercase);
        assert_eq!(value(&input, &mut cx), "let FOO = 1;  \nbar");
        cx.dispatch_action(super::TrimTrailingWhitespace);
        assert_eq!(value(&input, &mut cx), "let FOO = 1;\nbar");
        cx.dispatch_action(super::JoinLines);
        assert_eq!(value(&input, &mut cx), "let FOO = 1; bar");
        undo(&input, &mut cx);
        assert_eq!(value(&input, &mut cx), "let FOO = 1;\nbar");

        set_text(&input, &mut cx, "x(a)\ny(b)", 2);
        cx.update(|_, cx| {
            input.update(cx, |state, cx| state.set_selected_ranges([2..2, 7..7], cx));
        });
        cx.dispatch_action(super::RemoveSurroundingBrackets);
        assert_eq!(value(&input, &mut cx), "xa\nyb");

        // Two cursors on one line open one new line.
        set_text(&input, &mut cx, "ab\ncd", 0);
        cx.update(|_, cx| {
            input.update(cx, |state, cx| state.set_selected_ranges([0..0, 1..1], cx));
        });
        cx.simulate_keystrokes(&secondary("enter"));
        assert_eq!(value(&input, &mut cx), "ab\n\ncd");
    }

    #[test]
    fn test_key_context_names() {
        assert_eq!(
            super::INPUT_CODE_EDITOR_KEY_CONTEXT,
            format!("{} {}", crate::input::CONTEXT, super::CODE_EDITOR_CONTEXT)
        );
    }
}
