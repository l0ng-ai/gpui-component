//! A small whole-text scanner for strings and comments, used when the editor
//! has no syntax tree (an unregistered language, or a parse that has not
//! landed yet). Unlike a line-local guess it follows block comments and
//! multi-line strings across lines.
use std::ops::Range;

use super::LanguageConfig;

/// A string or comment in the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Span {
    /// Byte range, delimiters included.
    pub range: Range<usize>,
    pub comment: bool,
    /// False when the text (or, for a single-line string, the line) ended
    /// before the closing delimiter did.
    pub closed: bool,
}

/// Every string and comment in `text`, in order and non-overlapping.
pub(crate) fn scan(text: &str, config: &LanguageConfig) -> Vec<Span> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut ix = 0;
    while ix < text.len() {
        let rest = &text[ix..];
        let c = rest.chars().next().unwrap_or('\0');

        if let Some(token) = config.line_comment
            && rest.starts_with(token)
        {
            let end = rest.find('\n').map_or(text.len(), |n| ix + n);
            spans.push(Span {
                range: ix..end,
                comment: true,
                closed: true,
            });
            ix = end;
            continue;
        }

        if let Some((open, close)) = config.block_comment
            && rest.starts_with(open)
        {
            let (end, closed) = match rest[open.len()..].find(close) {
                Some(at) => (ix + open.len() + at + close.len(), true),
                None => (text.len(), false),
            };
            spans.push(Span {
                range: ix..end,
                comment: true,
                closed,
            });
            ix = end;
            continue;
        }

        if config.pairs_quote(c) {
            let triple: String = [c, c, c].iter().collect();
            let span = if c != '`' && rest.starts_with(&triple) {
                // Python's `"""` and `'''`: always multi-line.
                match rest[3..].find(&triple) {
                    Some(at) => Span {
                        range: ix..ix + 3 + at + 3,
                        comment: false,
                        closed: true,
                    },
                    None => Span {
                        range: ix..text.len(),
                        comment: false,
                        closed: false,
                    },
                }
            } else {
                let multi_line = c == '`' || config.multi_line_strings;
                scan_string(bytes, ix, c as u8, multi_line)
            };
            ix = span.range.end;
            spans.push(span);
            continue;
        }

        // A char literal in a language whose `'` is not a string quote
        // (Rust): `'x'`, `'\n'`, `'"'`. A lifetime (`'a`) has no closing
        // quote right after, so it is left alone.
        if c == '\'' {
            if let Some(len) = char_literal_len(rest) {
                spans.push(Span {
                    range: ix..ix + len,
                    comment: false,
                    closed: true,
                });
                ix += len;
                continue;
            }
        }

        ix += c.len_utf8();
    }
    spans
}

fn scan_string(bytes: &[u8], start: usize, quote: u8, multi_line: bool) -> Span {
    let mut ix = start + 1;
    while ix < bytes.len() {
        match bytes[ix] {
            b'\\' => ix += 2,
            b'\n' if !multi_line => {
                return Span {
                    range: start..ix,
                    comment: false,
                    closed: false,
                };
            }
            b if b == quote => {
                return Span {
                    range: start..ix + 1,
                    comment: false,
                    closed: true,
                };
            }
            _ => ix += 1,
        }
    }
    Span {
        range: start..bytes.len(),
        comment: false,
        closed: false,
    }
}

fn char_literal_len(rest: &str) -> Option<usize> {
    let mut chars = rest.char_indices().skip(1);
    let (_, first) = chars.next()?;
    if first == '\\' {
        // `'\n'`, `'\''`, `'\u{1F600}'`: the closing quote within a few chars.
        let body = &rest[2..];
        let skip_escaped = body.chars().next()?.len_utf8();
        let close = body[skip_escaped..].find('\'')?;
        if close > 10 || body[skip_escaped..skip_escaped + close].contains('\n') {
            return None;
        }
        return Some(2 + skip_escaped + close + 1);
    }
    if first == '\n' || first == '\'' {
        return None;
    }
    let (at, second) = chars.next()?;
    (second == '\'').then_some(at + 1)
}

/// The span containing the byte at `offset`, if any.
pub(crate) fn span_at(spans: &[Span], offset: usize) -> Option<&Span> {
    let ix = spans.partition_point(|s| s.range.end <= offset);
    spans.get(ix).filter(|s| s.range.start <= offset)
}

/// Whether the byte at `offset` is inside a string or comment.
pub(crate) fn offset_in_span(spans: &[Span], offset: usize) -> bool {
    span_at(spans, offset).is_some()
}

/// Whether a cursor at `offset` is inside a string (strictly between its
/// delimiters, or anywhere after the opener of an unterminated one) or a
/// comment (whose end it may touch).
pub(crate) fn cursor_in_span(spans: &[Span], offset: usize) -> bool {
    let ix = spans.partition_point(|s| s.range.end < offset);
    spans
        .get(ix)
        .is_some_and(|s| s.range.start < offset && (offset < s.range.end || s.comment || !s.closed))
}

#[cfg(test)]
mod tests {
    use super::super::language_config;
    use super::*;

    fn kinds<'a>(text: &'a str, lang: &str) -> Vec<(&'a str, bool)> {
        scan(text, &language_config(lang))
            .into_iter()
            .map(|s| (&text[s.range], s.comment))
            .collect()
    }

    #[test]
    fn test_scan_comments_and_strings() {
        assert_eq!(
            kinds("a // c\nb /* x\ny */ \"s\" z", "rust"),
            vec![("// c", true), ("/* x\ny */", true), ("\"s\"", false)]
        );
        // A comment marker inside a string is not a comment.
        assert_eq!(kinds("\"// no\" x", "rust"), vec![("\"// no\"", false)]);
        // Escapes.
        assert_eq!(kinds(r#""a\"b" c"#, "rust"), vec![(r#""a\"b""#, false)]);
    }

    #[test]
    fn test_scan_multi_line_strings() {
        // Rust strings may span lines.
        assert_eq!(kinds("\"a\nb\" c", "rust"), vec![("\"a\nb\"", false)]);
        // Python ones may not, but triple-quoted ones do.
        assert_eq!(kinds("'a\nb'", "python"), vec![("'a", false), ("'", false)]);
        assert_eq!(
            kinds("x = \"\"\"a\n'b'\n\"\"\" # c", "python"),
            vec![("\"\"\"a\n'b'\n\"\"\"", false), ("# c", true)]
        );
        // Template literals.
        assert_eq!(kinds("`a\n${b}`", "typescript"), vec![("`a\n${b}`", false)]);
    }

    #[test]
    fn test_scan_rust_char_literals_and_lifetimes() {
        assert_eq!(
            kinds("fn f<'a>(x: &'a str) { '\"'; '\\''; '(' }", "rust"),
            vec![("'\"'", false), ("'\\''", false), ("'('", false)]
        );
    }

    #[test]
    fn test_unterminated() {
        let config = language_config("rust");
        let spans = scan("a /* b\nc", &config);
        assert_eq!(spans.len(), 1);
        assert!(!spans[0].closed);
        assert!(cursor_in_span(&spans, 8));
        let spans = scan("x = \"abc", &config);
        assert!(cursor_in_span(&spans, 8));
    }

    #[test]
    fn test_queries() {
        let text = "a \"bc\" // d";
        let spans = scan(text, &language_config("rust"));
        assert!(!offset_in_span(&spans, 0));
        assert!(offset_in_span(&spans, 2));
        assert!(offset_in_span(&spans, 5));
        assert!(!offset_in_span(&spans, 6));
        assert!(offset_in_span(&spans, 7));
        // Cursor: before the opener, inside, after the closer, in the comment.
        assert!(!cursor_in_span(&spans, 2));
        assert!(cursor_in_span(&spans, 3));
        assert!(cursor_in_span(&spans, 5));
        assert!(!cursor_in_span(&spans, 6));
        assert!(cursor_in_span(&spans, text.len()));
    }
}
