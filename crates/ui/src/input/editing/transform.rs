//! Text transforms: case, trailing whitespace, joining lines, and removing
//! the brackets around a selection.
use std::ops::Range;

use ropey::Rope;

use super::{
    EditPlan, LinesPlan, closer_for, compose, compose_lines, is_closer, leading_whitespace,
    line_string, map_selection, matching_bracket, row_blocks, without_cr,
};
use crate::input::RopeExt as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Case {
    Upper,
    Lower,
    /// The first letter of every word upper-cased, the rest left alone.
    Title,
}

pub(crate) fn transform_case(text: &str, case: Case) -> String {
    match case {
        Case::Upper => text.to_uppercase(),
        Case::Lower => text.to_lowercase(),
        Case::Title => {
            let mut out = String::with_capacity(text.len());
            let mut at_word_start = true;
            for c in text.chars() {
                if at_word_start && c.is_alphabetic() {
                    out.extend(c.to_uppercase());
                } else {
                    out.push(c);
                }
                // An apostrophe keeps `don't` one word.
                at_word_start = !(c.is_alphanumeric() || c == '\'' || c == '_');
            }
            out
        }
    }
}

/// Change the case of the selection, or of the word under a bare cursor.
pub(crate) fn case_plan(text: &Rope, selection: &Range<usize>, case: Case) -> Option<EditPlan> {
    let range = if selection.is_empty() {
        text.word_range(selection.start)?
    } else {
        selection.clone()
    };
    let old = text.slice(range.clone()).to_string();
    let new = transform_case(&old, case);
    if new == old {
        return None;
    }
    let new_selection = if selection.is_empty() {
        let caret = (selection.start - range.start).min(new.len()) + range.start;
        // Stay on a char boundary if the word changed length.
        let caret = (range.start..=caret)
            .rev()
            .find(|&o| new.is_char_boundary(o - range.start))
            .unwrap_or(range.start);
        caret..caret
    } else {
        range.start..range.start + new.len()
    };
    Some(EditPlan {
        range,
        new_text: new,
        selection: new_selection,
    })
}

/// Remove trailing spaces and tabs from every line of the text.
pub(crate) fn trim_trailing_whitespace(
    text: &Rope,
    selections: &[Range<usize>],
) -> Option<LinesPlan> {
    let mut edits = Vec::new();
    for row in 0..text.lines_len() {
        let line = line_string(text, row);
        let content = without_cr(&line);
        let trimmed = content.trim_end_matches([' ', '\t']);
        if trimmed.len() < content.len() {
            let start = text.line_start_offset(row);
            edits.push((start + trimmed.len()..start + content.len(), String::new()));
        }
    }
    compose_lines(text, &edits, selections)
}

/// Join each selected block of lines into one; a bare cursor or one-line
/// selection joins its line with the next. The line break and the next
/// line's indent become a single space (none next to an empty line or before
/// a closing bracket). A bare cursor lands where the lines were joined.
pub(crate) fn join_lines(text: &Rope, selections: &[Range<usize>]) -> Option<LinesPlan> {
    let (blocks, owner) = row_blocks(text, selections);
    let total = text.lines_len();
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut first_join = Vec::with_capacity(blocks.len());
    for rows in &blocks {
        let last = if rows.len() == 1 {
            rows.start + 1
        } else {
            rows.end - 1
        };
        let last = last.min(total.saturating_sub(1));
        first_join.push(edits.len());
        for row in rows.start..last {
            let line = line_string(text, row);
            let content = without_cr(&line);
            let kept = content.trim_end_matches([' ', '\t']);
            let next = line_string(text, row + 1);
            let next_body = &without_cr(&next)[leading_whitespace(&next).len()..];
            let start = text.line_start_offset(row) + kept.len();
            let end = text.line_start_offset(row + 1) + leading_whitespace(&next).len();
            let space =
                !kept.is_empty() && !next_body.is_empty() && !next_body.starts_with(is_closer);
            edits.push((start..end, if space { " " } else { "" }.to_string()));
        }
    }
    if edits.is_empty() {
        return None;
    }

    let mut plan = compose_lines(text, &edits, selections)?;
    for (ix, selection) in selections.iter().enumerate() {
        if !selection.is_empty() {
            continue;
        }
        let Some((range, _)) = edits.get(first_join[owner[ix]]) else {
            continue;
        };
        // The join point, carried through the joins before it.
        let caret = map_selection(&edits, &(range.start..range.start)).start;
        plan.selections[ix] = caret..caret;
    }
    Some(plan)
}

/// Delete the innermost bracket pair around the selection (or cursor),
/// keeping what was inside selected where it was.
pub(crate) fn unwrap_brackets(
    text: &Rope,
    selection: &Range<usize>,
    skip: &dyn Fn(usize) -> bool,
) -> Option<EditPlan> {
    const MAX_SCAN: usize = 64 * 1024;
    let mut pending: Vec<char> = Vec::new();
    let mut offset = selection.start;
    let mut open = None;
    for c in text.chars_at(selection.start).reversed() {
        offset -= c.len_utf8();
        if selection.start - offset > MAX_SCAN {
            return None;
        }
        if skip(offset) {
            continue;
        }
        if is_closer(c) {
            pending.push(c);
        } else if let Some(close) = closer_for(c) {
            match pending.last() {
                Some(&p) if p == close => {
                    pending.pop();
                }
                Some(_) => return None,
                None => {
                    open = Some(offset);
                    break;
                }
            }
        }
    }
    let open = open?;
    let close = matching_bracket(text, open, skip)?;
    if close < selection.end {
        return None;
    }
    let edits = [
        (open..open + 1, String::new()),
        (close..close + 1, String::new()),
    ];
    compose(text, &edits, selection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transform_case() {
        assert_eq!(transform_case("Hello World", Case::Upper), "HELLO WORLD");
        assert_eq!(transform_case("Hello World", Case::Lower), "hello world");
        assert_eq!(
            transform_case("hello wORLD don't foo_bar x-y", Case::Title),
            "Hello WORLD Don't Foo_bar X-Y"
        );
        assert_eq!(transform_case("straße", Case::Upper), "STRASSE");
    }

    #[test]
    fn test_case_plan() {
        let text = Rope::from("let foo = bar;");
        // A bare cursor works on its word and stays put.
        let plan = case_plan(&text, &(5..5), Case::Upper).unwrap();
        assert_eq!((plan.range.clone(), plan.new_text.as_str()), (4..7, "FOO"));
        assert_eq!(plan.selection, 5..5);
        // A selection stays selected.
        let plan = case_plan(&text, &(0..7), Case::Title).unwrap();
        assert_eq!(plan.new_text, "Let Foo");
        assert_eq!(plan.selection, 0..7);
        // Nothing to change.
        assert_eq!(case_plan(&text, &(0..3), Case::Lower), None);
        // A selection that grows keeps covering the new text.
        let text = Rope::from("ß");
        let plan = case_plan(&text, &(0..2), Case::Upper).unwrap();
        assert_eq!(plan.selection, 0..2);
    }

    fn apply(text: &str, plan: &LinesPlan) -> String {
        let mut out = text[..plan.range.start].to_string();
        out.push_str(&plan.new_text);
        out.push_str(&text[plan.range.end..]);
        out
    }

    #[test]
    fn test_trim_trailing_whitespace() {
        let text = "a  \n\tb\t\n  \nc \r\nd";
        let plan = trim_trailing_whitespace(&Rope::from(text), &[3..3]).unwrap();
        assert_eq!(apply(text, &plan), "a\n\tb\n\nc\r\nd");
        // A cursor in the trimmed whitespace lands at the new line end.
        assert_eq!(plan.selections, vec![1..1]);
        assert_eq!(trim_trailing_whitespace(&Rope::from("a\nb"), &[0..0]), None);
    }

    fn join(text: &str, selections: &[Range<usize>]) -> Option<(String, Vec<Range<usize>>)> {
        let plan = join_lines(&Rope::from(text), selections)?;
        Some((apply(text, &plan), plan.selections))
    }

    #[test]
    fn test_join_lines() {
        assert_eq!(
            join("foo  \n    bar\nbaz", &[1..1]).unwrap(),
            ("foo bar\nbaz".to_string(), vec![3..3])
        );
        // A multi-line selection joins its lines, and stays selected.
        assert_eq!(
            join("a\n  b\n  c\nd", &[0..9]).unwrap(),
            ("a b c\nd".to_string(), vec![0..5])
        );
        // No space next to an empty line or before a closer.
        assert_eq!(join("a\n\nb", &[0..0]).unwrap().0, "a\nb");
        assert_eq!(join("f(\n  x\n)", &[4..8]).unwrap().0, "f(\n  x)");
        // The last line has nothing to join.
        assert_eq!(join("a\nb", &[2..2]), None);
        // Several cursors.
        assert_eq!(
            join("a\nb\nc\nd", &[0..0, 4..4]).unwrap(),
            ("a b\nc d".to_string(), vec![1..1, 5..5])
        );
    }

    #[test]
    fn test_unwrap_brackets() {
        let run = |text: &str, sel: Range<usize>| {
            let plan = unwrap_brackets(&Rope::from(text), &sel, &|_| false)?;
            let mut out = text[..plan.range.start].to_string();
            out.push_str(&plan.new_text);
            out.push_str(&text[plan.range.end..]);
            Some((out, plan.selection))
        };
        assert_eq!(run("f(a, b)", 3..3).unwrap(), ("fa, b".to_string(), 2..2));
        assert_eq!(run("f(a, b)", 2..6).unwrap(), ("fa, b".to_string(), 1..5));
        // Skips balanced pairs on the way out.
        assert_eq!(run("[x(1) |y]", 7..7).unwrap().0, "x(1) |y");
        assert_eq!(run("{ [a] }", 6..6).unwrap().0, " [a] ");
        // Nothing around the cursor.
        assert_eq!(run("a (b) c", 6..6), None);
        // The selection runs past the closer.
        assert_eq!(run("(a) b", 1..5), None);
    }
}
