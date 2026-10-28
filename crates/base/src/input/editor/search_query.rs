//! The query of a search: literal or a regular expression, matching case or
//! not, whole words only or not (VS Code's Aa / ab / .* toggles), and the
//! replacement it expands (`$1`, `${name}`, `$&`, `\n` with a regex).
//!
//! One matcher for the editor's Find and for any other search over text (a
//! Find in Files can share it, so both find the same matches).

use regex::{Captures, Regex, RegexBuilder};
use std::ops::Range;

/// The toggles of a search.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SearchOptions {
    /// Aa: upper and lower case are different letters.
    pub case_sensitive: bool,
    /// ab: a match must not have a word character right before or after it.
    pub whole_word: bool,
    /// .*: the query is a regular expression (Rust `regex` syntax, `^` / `$`
    /// at line ends, `.` does not cross lines, `\n` matches a line break).
    pub regex: bool,
}

/// A compiled query. Cheap to clone (the regex is shared) and `Send`, so a
/// background scan can take a copy.
#[derive(Clone, Debug)]
pub struct SearchQuery {
    regex: Regex,
    options: SearchOptions,
    /// For a literal query, its byte length: no match is longer.
    literal_len: Option<usize>,
}

/// How far a regex match is assumed to reach past where a batch ends (a
/// regex can match any length; batches look this far ahead).
pub const REGEX_REACH: usize = 64 << 10;
/// How much text before a batch is shown to the regex, so `^`, `\b` and
/// whole words see the character before the batch.
const LOOK_BEHIND: usize = 16;

impl SearchQuery {
    /// `Ok(None)` for an empty query; `Err` with the parser's message for an
    /// invalid regular expression.
    pub fn new(query: &str, options: SearchOptions) -> Result<Option<Self>, String> {
        if query.is_empty() {
            return Ok(None);
        }
        let pattern = if options.regex {
            query.to_string()
        } else {
            regex::escape(query)
        };
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(!options.case_sensitive)
            .multi_line(true)
            .size_limit(64 << 20)
            .build()
            .map_err(|err| short_error(&err))?;
        Ok(Some(Self {
            regex,
            options,
            literal_len: (!options.regex).then_some(query.len()),
        }))
    }

    pub fn options(&self) -> SearchOptions {
        self.options
    }

    pub fn regex(&self) -> &Regex {
        &self.regex
    }

    /// The longest a match can be (a regex: [`REGEX_REACH`]).
    pub fn max_match_len(&self) -> usize {
        self.literal_len.unwrap_or(REGEX_REACH)
    }

    /// The first non-empty match that starts at or after `start` (a char
    /// boundary of `haystack`), honouring whole words.
    pub fn find_at(&self, haystack: &str, start: usize) -> Option<Range<usize>> {
        let mut at = start;
        while at <= haystack.len() {
            let m = self.regex.find_at(haystack, at)?;
            if m.start() == m.end() {
                // An empty match (`a*`, `^`) finds nothing to show: go on.
                at = next_char(haystack, m.end())?;
                continue;
            }
            if !self.options.whole_word || is_whole_word(haystack, m.range()) {
                return Some(m.range());
            }
            at = next_char(haystack, m.start())?;
        }
        None
    }

    /// Every match in `haystack`, in order.
    pub fn find_iter<'a>(&'a self, haystack: &'a str) -> impl Iterator<Item = Range<usize>> + 'a {
        let mut at = Some(0);
        std::iter::from_fn(move || {
            let range = self.find_at(haystack, at?)?;
            at = Some(range.end);
            Some(range)
        })
    }

    /// The text that replaces the match at `range` of `haystack`: the
    /// template itself for a literal query; with a regex, `$1` / `${name}` /
    /// `$&` / `$$` and `\n` / `\t` / `\\` expanded (VS Code).
    pub fn replacement(&self, haystack: &str, range: Range<usize>, template: &str) -> String {
        if !self.options.regex {
            return template.to_string();
        }
        match self.regex.captures_at(haystack, range.start) {
            Some(caps) if caps.get(0).map(|m| m.range()) == Some(range.clone()) => {
                expand_replacement(&caps, template)
            }
            // Not the same match (should not happen): no groups to fill.
            _ => expand_replacement_without_groups(&haystack[range], template),
        }
    }
}

/// Matches of `query` that start in `from..to` of `text` (both clipped to
/// char boundaries), and where the next batch should start: at `to`, or past
/// a match that runs over it, so batches together find what one scan would.
pub fn find_in_str(
    text: &str,
    query: &SearchQuery,
    from: usize,
    to: usize,
) -> (Vec<Range<usize>>, usize) {
    let len = text.len();
    let from = floor_char(text, from.min(len));
    let to = ceil_char(text, to.min(len)).max(from);
    let ctx = floor_char(text, from.saturating_sub(LOOK_BEHIND));
    // One char more than the longest match, so a whole-word check sees the
    // character after a match that ends at the batch's reach.
    let end = ceil_char(text, (to + query.max_match_len() + 4).min(len));
    let haystack = &text[ctx..end];
    let mut found = Vec::new();
    let mut next = to;
    for m in FindFrom::new(query, haystack, from - ctx) {
        let range = ctx + m.start..ctx + m.end;
        if range.start >= to {
            break;
        }
        next = next.max(range.end);
        found.push(range);
    }
    (found, next)
}

struct FindFrom<'a> {
    query: &'a SearchQuery,
    haystack: &'a str,
    at: Option<usize>,
}

impl<'a> FindFrom<'a> {
    fn new(query: &'a SearchQuery, haystack: &'a str, at: usize) -> Self {
        Self {
            query,
            haystack,
            at: Some(at),
        }
    }
}

impl Iterator for FindFrom<'_> {
    type Item = Range<usize>;
    fn next(&mut self) -> Option<Range<usize>> {
        let range = self.query.find_at(self.haystack, self.at?)?;
        self.at = Some(range.end);
        Some(range)
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_whole_word(haystack: &str, range: Range<usize>) -> bool {
    let before = haystack[..range.start].chars().next_back();
    let first = haystack[range.clone()].chars().next();
    let last = haystack[range.clone()].chars().next_back();
    let after = haystack[range.end..].chars().next();
    // A boundary is only needed where the match itself has a word char.
    let open = !(before.is_some_and(is_word_char) && first.is_some_and(is_word_char));
    let close = !(after.is_some_and(is_word_char) && last.is_some_and(is_word_char));
    open && close
}

fn next_char(s: &str, at: usize) -> Option<usize> {
    if at >= s.len() {
        return None;
    }
    Some(at + s[at..].chars().next().map_or(1, char::len_utf8))
}

fn floor_char(s: &str, mut at: usize) -> usize {
    while at > 0 && !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn ceil_char(s: &str, mut at: usize) -> usize {
    while at < s.len() && !s.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// The parser's message without the pattern echo: one line for a tooltip.
fn short_error(err: &regex::Error) -> String {
    match err {
        regex::Error::Syntax(text) => text
            .lines()
            .rev()
            .find(|line| line.starts_with("error:"))
            .map(|line| line.trim_start_matches("error:").trim().to_string())
            .unwrap_or_else(|| text.lines().last().unwrap_or_default().to_string()),
        regex::Error::CompiledTooBig(_) => "the regular expression is too big".into(),
        other => other.to_string(),
    }
}

/// Expand a replacement template against a match (VS Code's rules): `$&`
/// and `$0` the whole match, `$1`…`$99` a group (two digits only when that
/// group exists), `${name}` a named group, `$$` a dollar; `\n`, `\t`, `\\`.
/// A group that did not take part is empty; anything else stays as typed.
pub fn expand_replacement(caps: &Captures, template: &str) -> String {
    expand(
        template,
        |group| match group {
            Group::Index(ix) => Some(caps.get(ix).map_or("", |m| m.as_str())),
            Group::Name(name) => caps.name(name).map(|m| m.as_str()),
        },
        caps.len(),
    )
}

fn expand_replacement_without_groups(whole: &str, template: &str) -> String {
    expand(
        template,
        |group| match group {
            Group::Index(0) => Some(whole),
            _ => None,
        },
        1,
    )
}

enum Group<'a> {
    Index(usize),
    Name(&'a str),
}

fn expand<'t, 'h>(
    template: &'t str,
    get: impl Fn(Group<'t>) -> Option<&'h str>,
    groups: usize,
) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => {
                let esc = match bytes[i + 1] {
                    b'n' => Some('\n'),
                    b't' => Some('\t'),
                    b'\\' => Some('\\'),
                    _ => None,
                };
                if let Some(c) = esc {
                    out.push(c);
                    i += 2;
                    continue;
                }
                out.push('\\');
                i += 1;
            }
            b'$' if i + 1 < bytes.len() => {
                let next = bytes[i + 1];
                if next == b'$' {
                    out.push('$');
                    i += 2;
                } else if next == b'&' {
                    out.push_str(get(Group::Index(0)).unwrap_or_default());
                    i += 2;
                } else if next.is_ascii_digit() {
                    let one = (next - b'0') as usize;
                    let two = bytes
                        .get(i + 2)
                        .filter(|b| b.is_ascii_digit())
                        .map(|b| one * 10 + (b - b'0') as usize);
                    let (ix, used) = match two {
                        Some(two) if two < groups => (two, 3),
                        _ => (one, 2),
                    };
                    if ix < groups {
                        out.push_str(get(Group::Index(ix)).unwrap_or_default());
                    } else {
                        out.push_str(&template[i..i + used]);
                    }
                    i += used;
                } else if next == b'{' {
                    match template[i + 2..].find('}') {
                        Some(close) => {
                            let name = &template[i + 2..i + 2 + close];
                            let value = match name.parse::<usize>() {
                                Ok(ix) if ix < groups => get(Group::Index(ix)),
                                Ok(_) => None,
                                Err(_) => get(Group::Name(name)),
                            };
                            match value {
                                Some(value) => out.push_str(value),
                                None => out.push_str(&template[i..i + 3 + close]),
                            }
                            i += 3 + close;
                        }
                        None => {
                            out.push('$');
                            i += 1;
                        }
                    }
                } else {
                    out.push('$');
                    i += 1;
                }
            }
            _ => {
                let c = template[i..].chars().next().unwrap();
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(query: &str, case_sensitive: bool, whole_word: bool, regex: bool) -> SearchQuery {
        SearchQuery::new(
            query,
            SearchOptions {
                case_sensitive,
                whole_word,
                regex,
            },
        )
        .unwrap()
        .unwrap()
    }

    fn all(query: &SearchQuery, text: &str) -> Vec<Range<usize>> {
        query.find_iter(text).collect()
    }

    #[test]
    fn a_literal_query_is_not_a_pattern() {
        let text = "a.b axb A.B";
        assert_eq!(all(&q("a.b", false, false, false), text), [0..3, 8..11]);
        assert_eq!(all(&q("a.b", true, false, false), text), [0..3]);
        assert_eq!(
            all(&q("a.b", false, false, true), text),
            [0..3, 4..7, 8..11]
        );
    }

    #[test]
    fn case_folding_is_unicode() {
        assert_eq!(
            all(&q("ärger", false, false, false), "ÄRGER ärger"),
            [0..6, 7..13]
        );
    }

    #[test]
    fn capture_groups_find_and_replace() {
        let text = "x = foo(a, b);\ny = foo(cc,dd);\nfoo()\n";
        let query = q(r"foo\((\w+), ?(\w+)\)", false, false, true);
        let found = all(&query, text);
        assert_eq!(found.len(), 2);
        let replaced: Vec<_> = found
            .iter()
            .map(|r| query.replacement(text, r.clone(), "bar($2, $1)"))
            .collect();
        assert_eq!(replaced, ["bar(b, a)", "bar(dd, cc)"]);
    }

    #[test]
    fn templates_expand_like_vs_code() {
        let query = q(r"(?P<k>\w+)=(\w+)", false, false, true);
        let text = "key=val";
        let r = 0..text.len();
        let t = |template| query.replacement(text, r.clone(), template);
        assert_eq!(t("${k}:$2"), "key:val");
        assert_eq!(t("$1a"), "keya", "$1 then a letter, not a group named 1a");
        assert_eq!(t("$&!"), "key=val!");
        assert_eq!(t("$0"), "key=val");
        assert_eq!(t("$$1"), "$1");
        assert_eq!(t(r"$1\n$2\t\\"), "key\nval\t\\");
        assert_eq!(
            t("$9 ${nope} $"),
            "$9 ${nope} $",
            "unknown groups stay as typed"
        );
        // A literal query never expands.
        let literal = q("key", false, false, false);
        assert_eq!(literal.replacement(text, 0..3, r"$1\n"), r"$1\n");
    }

    #[test]
    fn whole_words_skip_matches_inside_words() {
        let text = "foo food _foo foo.bar ffoo foo";
        assert_eq!(
            all(&q("foo", false, true, false), text),
            [0..3, 14..17, 27..30]
        );
        // A query that ends in a non-word char needs no boundary there.
        assert_eq!(all(&q("foo.", false, true, false), text), [14..18]);
        // The next candidate starts inside a rejected one.
        assert_eq!(all(&q("aa", false, true, false), "aaa aa"), [4..6]);
        assert_eq!(all(&q(r"\d+", false, true, true), "a1 22 3b"), [3..5]);
        assert_eq!(all(&q("中", false, true, false), "中文 中"), [7..10]);
    }

    #[test]
    fn line_anchors_and_newlines() {
        let text = "ab\nab\ncab\n";
        assert_eq!(all(&q("^ab", false, false, true), text), [0..2, 3..5]);
        assert_eq!(all(&q(r"b$", false, false, true), text), [1..2, 4..5, 8..9]);
        assert_eq!(all(&q(r"b\na", false, false, true), text), [1..4]);
    }

    #[test]
    fn empty_matches_are_skipped_without_looping() {
        assert_eq!(all(&q("x*", false, false, true), "axxb x"), [1..3, 5..6]);
        assert!(all(&q("^", false, false, true), "a\nb").is_empty());
    }

    #[test]
    fn an_invalid_regex_is_an_error_not_a_panic() {
        let options = SearchOptions {
            regex: true,
            ..Default::default()
        };
        for bad in ["(", "[a", "a{2,1}", "(?P<x", "\\"] {
            let err = SearchQuery::new(bad, options).unwrap_err();
            assert!(!err.is_empty() && !err.contains('\n'), "{bad:?}: {err:?}");
        }
        // The same text is a fine literal query.
        assert!(
            SearchQuery::new("(", SearchOptions::default())
                .unwrap()
                .is_some()
        );
        assert!(SearchQuery::new("", options).unwrap().is_none());
    }

    #[test]
    fn batches_find_what_one_scan_finds() {
        let text = "aa中aaa🙂aXa aaaa\naa中 foo(a,b) ".repeat(40);
        for (query, ww, re) in [
            ("aa", false, false),
            ("aa", true, false),
            ("中a", false, false),
            (r"a+", false, true),
            (r"^aa", false, true),
            (r"\bfoo\((\w),(\w)\)", false, true),
            (r"a\naa", false, true),
        ] {
            let query = q(query, false, ww, re);
            let whole = all(&query, &text);
            for batch in [1, 2, 3, 5, 7, 64, 10_000] {
                let mut found = Vec::new();
                let mut from = 0;
                while from < text.len() {
                    let (mut part, next) = find_in_str(&text, &query, from, from + batch);
                    assert!(next > from);
                    found.append(&mut part);
                    from = next;
                }
                assert_eq!(found, whole, "{:?} batch {batch}", query.regex());
            }
        }
    }

    #[test]
    fn a_regex_over_100k_lines_is_fast() {
        let text: String = (0..100_000)
            .map(|i| format!("let value_{i} = foo(a{i}, b{i}); // line {i}\n"))
            .collect();
        let query = q(r"foo\((\w+), ?(\w+)\)", false, true, true);
        let started = std::time::Instant::now();
        let count = query.find_iter(&text).count();
        let took = started.elapsed();
        assert_eq!(count, 100_000);
        // Generous for a debug build on a loaded CI runner; release is ~20 ms.
        let limit = if cfg!(debug_assertions) { 2_000 } else { 200 };
        assert!(took.as_millis() < limit, "regex find took {took:?}");
    }
}
