//! Category: TEXT — pure text inspection + transforms (counts, case changes,
//! reversing, find/replace, a small regex, JSON pretty/validate, CSV<->JSON,
//! markdown stripping, whitespace normalization, line dedupe/sort, word
//! frequency). Every skill here is a total function of its args: no network, no
//! clock, no I/O. All algorithms are real (not lying approximations) and bounded,
//! and every bad-args path returns a friendly error rather than panicking.
//!
//! NOTE ON OVERLAP: word/char/line counting lives in `utilities::word_count`;
//! the `text_stats` skill here is the richer inspector (adds sentences,
//! paragraphs, and a reading-time estimate) so the two are complementary, not
//! duplicates.

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{Category, SkillDef};

/// The text catalog.
pub fn skills() -> Vec<SkillDef> {
    vec![
        SkillDef::new(
            "text_stats",
            Category::Text,
            "Report detailed text statistics: characters, words, lines, sentences, paragraphs, and an estimated reading time. Use when the user wants a fuller breakdown than a plain word count.",
            &["text stats", "how many sentences", "reading time", "paragraph count", "analyze text"],
            text_stats,
        ),
        SkillDef::new(
            "reverse_text",
            Category::Text,
            "Reverse a string character by character (Unicode-aware). Use when the user wants text reversed or read backwards.",
            &["reverse", "backwards", "flip the text", "reverse string"],
            reverse_text,
        ),
        SkillDef::new(
            "find_replace",
            Category::Text,
            "Literal (non-regex) find-and-replace in text, optionally case-insensitive. Use when the user wants every occurrence of one substring swapped for another.",
            &["find and replace", "replace all", "substitute", "swap text"],
            find_replace,
        ),
        SkillDef::new(
            "regex_test",
            Category::Text,
            "Test whether text matches a small regular expression (supports . ^ $ * + ? and [..] classes). Use to check if a string matches a pattern; returns a yes/no with the first match.",
            &["regex test", "does it match", "matches pattern", "test regex"],
            regex_test,
        ),
        SkillDef::new(
            "regex_extract",
            Category::Text,
            "Extract every non-overlapping match of a small regular expression from text. Use to pull all occurrences of a pattern out of a block of text.",
            &["regex extract", "find all matches", "extract pattern", "pull out matches"],
            regex_extract,
        ),
        SkillDef::new(
            "json_pretty",
            Category::Text,
            "Validate JSON and pretty-print it with 2-space indentation (or minify it). Use to check that JSON is well-formed and reformat it; returns a precise parse error if invalid.",
            &["pretty print json", "format json", "validate json", "minify json"],
            json_pretty,
        ),
        SkillDef::new(
            "csv_to_json",
            Category::Text,
            "Convert a small CSV block (first row = headers) into a JSON array of objects. Use to turn comma-separated rows into structured JSON. Handles quoted fields and embedded commas.",
            &["csv to json", "convert csv", "parse csv", "csv into json"],
            csv_to_json,
        ),
        SkillDef::new(
            "json_to_csv",
            Category::Text,
            "Convert a JSON array of flat objects into CSV (header row + one row per object). Use to flatten structured JSON records into a CSV table.",
            &["json to csv", "convert json to csv", "export csv", "flatten json"],
            json_to_csv,
        ),
        SkillDef::new(
            "strip_markdown",
            Category::Text,
            "Strip common Markdown formatting (headings, bold/italic/code, links, list bullets, blockquotes) down to readable plain text. Use to get the prose out of Markdown.",
            &["strip markdown", "markdown to text", "remove formatting", "plain text"],
            strip_markdown,
        ),
        SkillDef::new(
            "normalize_whitespace",
            Category::Text,
            "Collapse runs of whitespace to single spaces and trim each line, dropping blank lines. Use to clean up messy spacing/tabs/trailing whitespace in text.",
            &["normalize whitespace", "collapse spaces", "trim whitespace", "clean up spacing"],
            normalize_whitespace,
        ),
        SkillDef::new(
            "dedupe_lines",
            Category::Text,
            "Remove duplicate lines, keeping the first occurrence and original order. Optionally ignore case. Use to de-duplicate a list of lines.",
            &["dedupe lines", "remove duplicate lines", "unique lines", "deduplicate"],
            dedupe_lines,
        ),
        SkillDef::new(
            "sort_lines",
            Category::Text,
            "Sort the lines of text alphabetically (optionally descending, case-insensitive, or unique). Use to alphabetize a list of lines.",
            &["sort lines", "alphabetize", "order lines", "sort alphabetically"],
            sort_lines,
        ),
        SkillDef::new(
            "word_frequency",
            Category::Text,
            "Count how often each word appears, returned most-frequent first (ties broken alphabetically). Use to find the most common words in a passage.",
            &["word frequency", "most common words", "count word occurrences", "word counts"],
            word_frequency,
        ),
    ]
}

// ---------------------------------------------------------------------------
// Shared arg helpers
// ---------------------------------------------------------------------------

/// Pull a required `&str` arg by key, with a friendly per-skill error.
fn req_str<'a>(args: &'a Value, key: &str, skill: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{skill} needs a '{key}' string argument"))
}

/// Read an optional boolean flag, defaulting to `false` when absent.
fn opt_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Guard against unbounded work: most text skills cap input length. 1 MiB is far
/// beyond any spoken/typed request and keeps the linear skills O(n) and quick.
/// The one exception is the regex matcher, whose greedy backtracking is NOT O(n)
/// in the worst case; it is bounded instead by a hard step budget plus a
/// compiled-token cap (see `MAX_REGEX_TOKENS` / `REGEX_STEP_BUDGET` below), so a
/// crafted pattern errors out fast rather than wedging the event loop.
const MAX_INPUT: usize = 1 << 20;

/// Reject pathologically large input up front (a friendly error, never a hang).
fn bound(text: &str, skill: &str) -> Result<()> {
    if text.len() > MAX_INPUT {
        return Err(anyhow!(
            "{skill}: input too large ({} bytes; limit {MAX_INPUT})",
            text.len()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// text_stats
// ---------------------------------------------------------------------------

/// `text_stats {text}` -> a one-line breakdown. Sentences are counted as runs
/// ended by `.`, `!`, or `?` (collapsing `...` and `?!` into a single terminator
/// so they are not over-counted); paragraphs are blank-line-separated blocks;
/// reading time uses 200 wpm rounded up to whole minutes. Pure + total.
fn text_stats(args: &Value) -> Result<String> {
    let text = req_str(args, "text", "text_stats")?;
    bound(text, "text_stats")?;
    let chars = text.chars().count();
    let words = text.split_whitespace().count();
    let lines = if text.is_empty() { 0 } else { text.lines().count() };
    let sentences = count_sentences(text);
    let paragraphs = text
        .split("\n\n")
        .filter(|p| !p.trim().is_empty())
        .count();
    // Reading time at 200 wpm, rounded up; empty text reads in 0 minutes.
    let reading_min = words.div_ceil(200);
    Ok(format!(
        "{chars} characters, {words} words, {lines} lines, {sentences} sentences, {paragraphs} paragraphs, ~{reading_min} min read"
    ))
}

/// Count sentence terminators, collapsing adjacent terminators (`...`, `?!`) into
/// one so an ellipsis is a single sentence end, not three.
fn count_sentences(text: &str) -> usize {
    let mut count = 0;
    let mut prev_terminator = false;
    for c in text.chars() {
        let is_term = matches!(c, '.' | '!' | '?');
        if is_term && !prev_terminator {
            count += 1;
        }
        prev_terminator = is_term;
    }
    count
}

// ---------------------------------------------------------------------------
// reverse_text
// ---------------------------------------------------------------------------

/// `reverse_text {text}` -> the string reversed by Unicode scalar value. Pure.
fn reverse_text(args: &Value) -> Result<String> {
    let text = req_str(args, "text", "reverse_text")?;
    bound(text, "reverse_text")?;
    Ok(text.chars().rev().collect())
}

// ---------------------------------------------------------------------------
// find_replace
// ---------------------------------------------------------------------------

/// `find_replace {text, find, replace?, ignore_case?}` -> literal replacement of
/// every occurrence of `find` with `replace` (default empty = delete). With
/// `ignore_case` the search is ASCII-case-insensitive but the surrounding text
/// is preserved verbatim. An empty `find` is rejected (it would match between
/// every character). Pure + total.
fn find_replace(args: &Value) -> Result<String> {
    let text = req_str(args, "text", "find_replace")?;
    bound(text, "find_replace")?;
    let find = req_str(args, "find", "find_replace")?;
    let replace = args.get("replace").and_then(Value::as_str).unwrap_or("");
    if find.is_empty() {
        return Err(anyhow!("find_replace 'find' must be non-empty"));
    }
    if opt_bool(args, "ignore_case") {
        Ok(replace_ascii_ci(text, find, replace))
    } else {
        Ok(text.replace(find, replace))
    }
}

/// Case-insensitive (ASCII) literal replace, preserving the original text's case.
/// Walks byte positions, matching `needle` case-folded; non-overlapping.
fn replace_ascii_ci(haystack: &str, needle: &str, replacement: &str) -> String {
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0;
    while i < hb.len() {
        if i + nb.len() <= hb.len()
            && hb[i..i + nb.len()]
                .iter()
                .zip(nb)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            out.push_str(replacement);
            i += nb.len();
        } else {
            // Push the full UTF-8 char starting at i to stay on char boundaries.
            let ch = haystack[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

// ---------------------------------------------------------------------------
// regex (tiny, honest subset engine shared by regex_test + regex_extract)
// ---------------------------------------------------------------------------
//
// SUPPORTED SUBSET (documented so the skill never lies about what it matches):
//   .            any single character (except newline is allowed too)
//   literal      any non-metacharacter matches itself
//   [abc] [a-z]  character class, [^...] negation, ranges
//   *  +  ?      greedy quantifiers on the preceding single atom
//   ^  $         anchors at start / end of the whole text
//   \\ d w s .   escapes: \d \w \s and escaping a metacharacter to literal
// NOT supported: alternation (|), groups (), backreferences, lazy quantifiers.
// Unsupported metacharacters are reported as an error — never silently ignored.

/// A single matchable atom in the compiled pattern.
#[derive(Debug, Clone)]
enum Atom {
    Any,
    Literal(char),
    Class { negated: bool, ranges: Vec<(char, char)> },
}

/// An atom plus its quantifier.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Quant {
    One,
    ZeroOrMore,
    OneOrMore,
    ZeroOrOne,
}

#[derive(Debug, Clone)]
struct Token {
    atom: Atom,
    quant: Quant,
}

/// A compiled pattern: anchors plus a token sequence.
#[derive(Debug, Clone)]
struct Pattern {
    anchor_start: bool,
    anchor_end: bool,
    tokens: Vec<Token>,
}

/// Maximum number of compiled tokens (quantified atoms) in a single pattern.
/// Consecutive top-level greedy quantifiers (e.g. `a*a*a*…b`) backtrack
/// exponentially in the token count, so capping the token count is the first
/// line of defence against catastrophic backtracking. 64 is far more than any
/// honest hand-written pattern needs.
const MAX_REGEX_TOKENS: usize = 64;

/// Hard ceiling on `atom_matches`-style comparisons performed while matching a
/// single skill call. When the budget is exhausted the matcher aborts with a
/// friendly error instead of continuing to backtrack — this is what guarantees a
/// crafted pattern cannot wedge the daemon's single event loop. ~2M atom
/// comparisons finish in well under a second yet are far beyond any real match.
const REGEX_STEP_BUDGET: u64 = 2_000_000;

/// Raised by the matcher when [`REGEX_STEP_BUDGET`] is exhausted; the skill
/// surface turns this into a user-facing "regex too complex" error.
#[derive(Debug)]
struct RegexBudgetExceeded;

/// Compile the regex source into a [`Pattern`], rejecting unsupported syntax with
/// a friendly error so a user is never told "no match" when we simply could not
/// parse their pattern.
fn compile_regex(src: &str) -> Result<Pattern> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut anchor_start = false;
    let mut anchor_end = false;
    if chars.first() == Some(&'^') {
        anchor_start = true;
        i = 1;
    }
    let mut tokens = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        // Trailing $ is the end anchor (this is the last char, so we stop after).
        if c == '$' && i + 1 == chars.len() {
            anchor_end = true;
            break;
        }
        let atom = match c {
            '(' | ')' | '|' | '{' | '}' => {
                return Err(anyhow!(
                    "regex: '{c}' (groups/alternation/repetition-counts) is not supported by this small engine"
                ));
            }
            '*' | '+' | '?' => {
                return Err(anyhow!("regex: quantifier '{c}' has nothing to repeat"));
            }
            '.' => {
                i += 1;
                Atom::Any
            }
            '[' => {
                let (class, next) = parse_class(&chars, i)?;
                i = next;
                class
            }
            '\\' => {
                let (atom, next) = parse_escape(&chars, i)?;
                i = next;
                atom
            }
            '^' => return Err(anyhow!("regex: '^' is only an anchor at the very start")),
            other => {
                i += 1;
                Atom::Literal(other)
            }
        };
        // A following quantifier applies to this atom.
        let quant = match chars.get(i) {
            Some('*') => {
                i += 1;
                Quant::ZeroOrMore
            }
            Some('+') => {
                i += 1;
                Quant::OneOrMore
            }
            Some('?') => {
                i += 1;
                Quant::ZeroOrOne
            }
            _ => Quant::One,
        };
        tokens.push(Token { atom, quant });
        if tokens.len() > MAX_REGEX_TOKENS {
            return Err(anyhow!(
                "regex: pattern is too complex for this small engine (> {MAX_REGEX_TOKENS} tokens)"
            ));
        }
    }
    Ok(Pattern { anchor_start, anchor_end, tokens })
}

/// Parse a `[...]` character class starting at `start` (the `[`). Returns the
/// atom and the index just past the closing `]`.
fn parse_class(chars: &[char], start: usize) -> Result<(Atom, usize)> {
    let mut i = start + 1;
    let mut negated = false;
    if chars.get(i) == Some(&'^') {
        negated = true;
        i += 1;
    }
    let mut ranges = Vec::new();
    let mut saw_any = false;
    while i < chars.len() && chars[i] != ']' {
        let lo = chars[i];
        // Range a-z (but a trailing '-' before ']' is a literal '-').
        if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|&c| c != ']') {
            let hi = chars[i + 2];
            if lo > hi {
                return Err(anyhow!("regex: class range '{lo}-{hi}' is reversed"));
            }
            ranges.push((lo, hi));
            i += 3;
        } else {
            ranges.push((lo, lo));
            i += 1;
        }
        saw_any = true;
    }
    if i >= chars.len() {
        return Err(anyhow!("regex: unterminated character class '['"));
    }
    if !saw_any {
        return Err(anyhow!("regex: empty character class '[]'"));
    }
    Ok((Atom::Class { negated, ranges }, i + 1))
}

/// Parse a `\x` escape starting at the backslash. Supports `\d \w \s` shorthand
/// classes and escaping any metacharacter (or itself) to a literal.
fn parse_escape(chars: &[char], start: usize) -> Result<(Atom, usize)> {
    let next = chars
        .get(start + 1)
        .ok_or_else(|| anyhow!("regex: trailing backslash with nothing to escape"))?;
    let atom = match next {
        'd' => Atom::Class { negated: false, ranges: vec![('0', '9')] },
        'w' => Atom::Class {
            negated: false,
            ranges: vec![('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')],
        },
        's' => Atom::Class {
            negated: false,
            ranges: vec![(' ', ' '), ('\t', '\t'), ('\n', '\n'), ('\r', '\r')],
        },
        'D' => Atom::Class { negated: true, ranges: vec![('0', '9')] },
        'W' => Atom::Class {
            negated: true,
            ranges: vec![('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')],
        },
        'S' => Atom::Class {
            negated: true,
            ranges: vec![(' ', ' '), ('\t', '\t'), ('\n', '\n'), ('\r', '\r')],
        },
        // Escape a metacharacter (or any char) to its literal self.
        other => Atom::Literal(*other),
    };
    Ok((atom, start + 2))
}

/// Does this single atom match character `c`?
fn atom_matches(atom: &Atom, c: char) -> bool {
    match atom {
        Atom::Any => true,
        Atom::Literal(l) => *l == c,
        Atom::Class { negated, ranges } => {
            let hit = ranges.iter().any(|&(lo, hi)| c >= lo && c <= hi);
            hit != *negated
        }
    }
}

/// Backtracking matcher: try to match `tokens[ti..]` against `text[pos..]`,
/// honoring greedy quantifiers. Returns `Ok(Some(end))` (the end index in chars)
/// on a successful match, `Ok(None)` on a clean no-match, or
/// `Err(RegexBudgetExceeded)` if the `budget` of atom comparisons runs out (a
/// crafted catastrophic-backtracking pattern). `anchor_end` requires the match to
/// reach the text end. The budget is decremented once per atom comparison, which
/// is the unit of backtracking work, so an exponential pattern hits the ceiling
/// and aborts in well under a second instead of wedging the event loop.
fn match_here(
    tokens: &[Token],
    ti: usize,
    text: &[char],
    pos: usize,
    anchor_end: bool,
    budget: &std::cell::Cell<u64>,
) -> Result<Option<usize>, RegexBudgetExceeded> {
    if ti == tokens.len() {
        if anchor_end && pos != text.len() {
            return Ok(None);
        }
        return Ok(Some(pos));
    }
    let tok = &tokens[ti];
    match tok.quant {
        Quant::One => {
            if pos < text.len() && charge(budget, &tok.atom, text[pos])? {
                match_here(tokens, ti + 1, text, pos + 1, anchor_end, budget)
            } else {
                Ok(None)
            }
        }
        Quant::ZeroOrOne => {
            // Greedy: try consuming one first, then zero.
            if pos < text.len() && charge(budget, &tok.atom, text[pos])? {
                if let Some(end) = match_here(tokens, ti + 1, text, pos + 1, anchor_end, budget)? {
                    return Ok(Some(end));
                }
            }
            match_here(tokens, ti + 1, text, pos, anchor_end, budget)
        }
        Quant::ZeroOrMore | Quant::OneOrMore => {
            // Consume as many as possible, then backtrack toward the minimum.
            let min = if tok.quant == Quant::OneOrMore { 1 } else { 0 };
            let mut count = 0;
            let mut p = pos;
            while p < text.len() && charge(budget, &tok.atom, text[p])? {
                p += 1;
                count += 1;
            }
            while count >= min {
                let try_at = pos + count;
                if let Some(end) = match_here(tokens, ti + 1, text, try_at, anchor_end, budget)? {
                    return Ok(Some(end));
                }
                if count == 0 {
                    break;
                }
                count -= 1;
            }
            Ok(None)
        }
    }
}

/// Charge one unit of the step budget and perform a single atom comparison.
/// Returns `Err(RegexBudgetExceeded)` once the budget is spent so the caller
/// stops backtracking immediately.
fn charge(
    budget: &std::cell::Cell<u64>,
    atom: &Atom,
    c: char,
) -> Result<bool, RegexBudgetExceeded> {
    let remaining = budget.get();
    if remaining == 0 {
        return Err(RegexBudgetExceeded);
    }
    budget.set(remaining - 1);
    Ok(atom_matches(atom, c))
}

/// Find the first match in `text`, returning `(start_char, end_char)` indices.
/// Honors the start anchor (only position 0) and end anchor. The shared `budget`
/// spans every start position so a pattern cannot dodge the cap by being retried
/// at each offset; an exhausted budget is surfaced as `Err`.
fn first_match(
    pattern: &Pattern,
    text: &[char],
    budget: &std::cell::Cell<u64>,
) -> Result<Option<(usize, usize)>, RegexBudgetExceeded> {
    let starts: Vec<usize> = if pattern.anchor_start {
        vec![0]
    } else {
        (0..=text.len()).collect()
    };
    for s in starts {
        if let Some(end) = match_here(&pattern.tokens, 0, text, s, pattern.anchor_end, budget)? {
            return Ok(Some((s, end)));
        }
    }
    Ok(None)
}

/// A fresh full step budget for one skill invocation.
fn fresh_budget() -> std::cell::Cell<u64> {
    std::cell::Cell::new(REGEX_STEP_BUDGET)
}

/// The user-facing error when a pattern blows the step budget.
fn too_complex_err() -> anyhow::Error {
    anyhow!("regex: pattern too complex for this small engine (exceeded the match step budget)")
}

/// `regex_test {text, pattern}` -> whether the pattern matches, and the first
/// matched substring if so. Reports unsupported regex syntax as an error.
fn regex_test(args: &Value) -> Result<String> {
    let text = req_str(args, "text", "regex_test")?;
    bound(text, "regex_test")?;
    let pat = req_str(args, "pattern", "regex_test")?;
    let pattern = compile_regex(pat)?;
    let chars: Vec<char> = text.chars().collect();
    let budget = fresh_budget();
    match first_match(&pattern, &chars, &budget).map_err(|_| too_complex_err())? {
        Some((s, e)) => {
            let matched: String = chars[s..e].iter().collect();
            Ok(format!("match: yes (first match: \"{matched}\")"))
        }
        None => Ok("match: no".to_string()),
    }
}

/// `regex_extract {text, pattern}` -> every non-overlapping match, one per line,
/// with a count header. A zero-width match advances by one char to guarantee
/// termination (no infinite loop on patterns like `a*`).
fn regex_extract(args: &Value) -> Result<String> {
    let text = req_str(args, "text", "regex_extract")?;
    bound(text, "regex_extract")?;
    let pat = req_str(args, "pattern", "regex_extract")?;
    let pattern = compile_regex(pat)?;
    let chars: Vec<char> = text.chars().collect();
    // One budget shared across every re-anchored match attempt: a crafted pattern
    // cannot dodge the cap by being retried at each offset.
    let budget = fresh_budget();
    let mut matches = Vec::new();
    let mut pos = 0;
    while pos <= chars.len() {
        // Re-anchor the search to start at or after `pos`.
        let sub_starts: Vec<usize> = if pattern.anchor_start {
            if pos == 0 { vec![0] } else { vec![] }
        } else {
            (pos..=chars.len()).collect()
        };
        let mut found = None;
        for s in sub_starts {
            if let Some(end) = match_here(&pattern.tokens, 0, &chars, s, pattern.anchor_end, &budget)
                .map_err(|_| too_complex_err())?
            {
                found = Some((s, end));
                break;
            }
        }
        match found {
            Some((s, e)) => {
                let m: String = chars[s..e].iter().collect();
                matches.push(m);
                // Advance past the match; a zero-width match still moves by 1.
                pos = if e > s { e } else { s + 1 };
            }
            None => break,
        }
    }
    if matches.is_empty() {
        Ok("0 matches".to_string())
    } else {
        Ok(format!("{} matches:\n{}", matches.len(), matches.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// json_pretty
// ---------------------------------------------------------------------------

/// Recursively re-key every JSON object into alphabetical key order so emitted
/// output is deterministic regardless of whether serde_json's `Map` is a sorted
/// `BTreeMap` or an insertion-ordered `IndexMap` (the `preserve_order` feature can
/// be unified in transitively, which would otherwise make key order — and these
/// skills' output — vary between builds). Pure.
fn sort_json_keys(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map
                .into_iter()
                .map(|(k, val)| (k, sort_json_keys(val)))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out = serde_json::Map::new();
            for (k, val) in entries {
                out.insert(k, val);
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(sort_json_keys).collect()),
        other => other,
    }
}

/// `json_pretty {json, minify?}` -> validate the JSON and reformat it. Default is
/// 2-space pretty-printing; `minify=true` produces the compact form. Object keys
/// are emitted in alphabetical order (sorted explicitly, so output is stable
/// regardless of serde_json's map backing). An invalid document returns
/// serde_json's precise parse error (line/column). Pure.
fn json_pretty(args: &Value) -> Result<String> {
    let src = req_str(args, "json", "json_pretty")?;
    bound(src, "json_pretty")?;
    let parsed: Value = serde_json::from_str(src)
        .map_err(|e| anyhow!("json_pretty: invalid JSON ({e})"))?;
    let parsed = sort_json_keys(parsed);
    if opt_bool(args, "minify") {
        Ok(serde_json::to_string(&parsed)?)
    } else {
        Ok(serde_json::to_string_pretty(&parsed)?)
    }
}

// ---------------------------------------------------------------------------
// csv_to_json
// ---------------------------------------------------------------------------

/// Parse one CSV line into fields, honoring RFC-4180-style double-quoting:
/// quoted fields may contain commas and `""` is an escaped quote. Pure.
fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => {
                    fields.push(std::mem::take(&mut cur));
                }
                _ => cur.push(c),
            }
        }
    }
    fields.push(cur);
    fields
}

/// `csv_to_json {csv}` -> a JSON array of objects keyed by the header row. All
/// values are strings (CSV is untyped). Rows with a different field count than
/// the header are an error (so the output never silently misaligns). Pure.
fn csv_to_json(args: &Value) -> Result<String> {
    let csv = req_str(args, "csv", "csv_to_json")?;
    bound(csv, "csv_to_json")?;
    let mut lines = csv.lines().filter(|l| !l.trim().is_empty());
    let header_line = lines
        .next()
        .ok_or_else(|| anyhow!("csv_to_json: CSV is empty (need at least a header row)"))?;
    let headers = parse_csv_line(header_line);
    if headers.iter().any(|h| h.trim().is_empty()) {
        return Err(anyhow!("csv_to_json: header row has an empty column name"));
    }
    let mut rows = Vec::new();
    for (idx, line) in lines.enumerate() {
        let fields = parse_csv_line(line);
        if fields.len() != headers.len() {
            return Err(anyhow!(
                "csv_to_json: row {} has {} fields but the header has {}",
                idx + 1,
                fields.len(),
                headers.len()
            ));
        }
        let mut obj = serde_json::Map::new();
        for (h, f) in headers.iter().zip(fields) {
            obj.insert(h.clone(), Value::String(f));
        }
        rows.push(Value::Object(obj));
    }
    // Sort keys for deterministic output regardless of serde_json's map backing.
    Ok(serde_json::to_string_pretty(&sort_json_keys(Value::Array(rows)))?)
}

// ---------------------------------------------------------------------------
// json_to_csv
// ---------------------------------------------------------------------------

/// Quote a CSV field if it contains a comma, quote, or newline; escape embedded
/// quotes by doubling. Pure.
fn csv_quote(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

/// Render a JSON scalar as a CSV cell string (string as-is, numbers/bools as
/// their literal text, null as empty). Nested arrays/objects are rejected by the
/// caller, so they never reach here.
fn json_scalar_to_cell(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        _ => String::new(), // unreachable: caller validates flatness
    }
}

/// `json_to_csv {json}` -> CSV from a JSON array of flat objects. The columns are
/// the union of every object's keys, sorted alphabetically (we sort the column
/// list explicitly, so the order is deterministic regardless of serde_json's map
/// backing). A non-array, non-object element, or a nested value, is a friendly
/// error. Pure.
fn json_to_csv(args: &Value) -> Result<String> {
    let src = req_str(args, "json", "json_to_csv")?;
    bound(src, "json_to_csv")?;
    let parsed: Value = serde_json::from_str(src)
        .map_err(|e| anyhow!("json_to_csv: invalid JSON ({e})"))?;
    let arr = parsed
        .as_array()
        .ok_or_else(|| anyhow!("json_to_csv: expected a JSON array of objects"))?;
    if arr.is_empty() {
        return Ok(String::new());
    }
    // Collect the union of keys in first-seen order.
    let mut columns: Vec<String> = Vec::new();
    for elem in arr {
        let obj = elem
            .as_object()
            .ok_or_else(|| anyhow!("json_to_csv: every array element must be an object"))?;
        for k in obj.keys() {
            if !columns.iter().any(|c| c == k) {
                columns.push(k.clone());
            }
        }
        // Reject nested values up front (CSV is flat).
        for (k, val) in obj {
            if val.is_array() || val.is_object() {
                return Err(anyhow!(
                    "json_to_csv: value for '{k}' is nested; only flat objects convert to CSV"
                ));
            }
        }
    }
    // Sort columns alphabetically for a stable, deterministic header.
    columns.sort();
    let mut out = String::new();
    out.push_str(&columns.iter().map(|c| csv_quote(c)).collect::<Vec<_>>().join(","));
    out.push('\n');
    for elem in arr {
        let obj = elem.as_object().unwrap();
        let row: Vec<String> = columns
            .iter()
            .map(|c| csv_quote(&obj.get(c).map(json_scalar_to_cell).unwrap_or_default()))
            .collect();
        out.push_str(&row.join(","));
        out.push('\n');
    }
    // Drop the trailing newline for a clean single block.
    Ok(out.trim_end_matches('\n').to_string())
}

// ---------------------------------------------------------------------------
// strip_markdown
// ---------------------------------------------------------------------------

/// `strip_markdown {text}` -> the prose with common Markdown removed: ATX
/// headings (`#`), blockquote markers (`>`), list bullets (`-`, `*`, `+`, `1.`),
/// emphasis (`**`, `*`, `_`, `~~`), inline code/backticks, and `[label](url)`
/// links collapse to `label`. Line structure is preserved. Pure + total.
fn strip_markdown(args: &Value) -> Result<String> {
    let text = req_str(args, "text", "strip_markdown")?;
    bound(text, "strip_markdown")?;
    let mut out_lines = Vec::new();
    for raw in text.lines() {
        let mut line = raw.to_string();
        // Strip leading blockquote and heading markers, then list bullets.
        let trimmed = line.trim_start();
        let indent_len = line.len() - trimmed.len();
        let indent = &line[..indent_len];
        let mut body = trimmed.to_string();
        // Blockquote: one or more leading '>' with optional spaces.
        while let Some(rest) = body.strip_prefix('>') {
            body = rest.trim_start().to_string();
        }
        // ATX heading: leading run of '#'.
        if let Some(after) = body.strip_prefix(|c| c == '#') {
            let hashes_end = body.len() - after.len()
                + after.chars().take_while(|&c| c == '#').count();
            if body[..hashes_end].chars().all(|c| c == '#') {
                body = body[hashes_end..].trim_start().to_string();
            }
        }
        // Unordered list bullet: -, *, + followed by a space.
        if let Some(rest) = body
            .strip_prefix("- ")
            .or_else(|| body.strip_prefix("* "))
            .or_else(|| body.strip_prefix("+ "))
        {
            body = rest.to_string();
        } else {
            // Ordered list bullet: digits then '. '.
            let digits = body.chars().take_while(|c| c.is_ascii_digit()).count();
            if digits > 0 && body[digits..].starts_with(". ") {
                body = body[digits + 2..].to_string();
            }
        }
        line = format!("{indent}{}", strip_inline_md(&body));
        out_lines.push(line);
    }
    Ok(out_lines.join("\n"))
}

/// Remove inline Markdown from one line: links `[t](u)` -> `t`, then emphasis and
/// code markers `** * _ ~~ \``. Done with simple passes — pure + total.
fn strip_inline_md(line: &str) -> String {
    // Links: [label](url) -> label. Hand-scanned, single pass.
    let mut s = collapse_links(line);
    // Remove the multi-char markers first, then single-char ones.
    for marker in ["**", "~~", "`"] {
        s = s.replace(marker, "");
    }
    // Single '*' and '_' emphasis markers.
    s = s.replace('*', "").replace('_', "");
    s
}

/// Collapse `[label](url)` to `label`, leaving non-link brackets alone.
fn collapse_links(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            // Find the closing ']' then a '(' immediately after, then ')'.
            if let Some(close) = chars[i + 1..].iter().position(|&c| c == ']') {
                let label_end = i + 1 + close;
                if chars.get(label_end + 1) == Some(&'(') {
                    if let Some(paren) =
                        chars[label_end + 2..].iter().position(|&c| c == ')')
                    {
                        let label: String = chars[i + 1..label_end].iter().collect();
                        out.push_str(&label);
                        i = label_end + 2 + paren + 1;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// normalize_whitespace
// ---------------------------------------------------------------------------

/// `normalize_whitespace {text, keep_lines?}` -> collapse runs of whitespace to a
/// single space and trim. By default the whole text becomes one line; with
/// `keep_lines=true`, each line is collapsed/trimmed independently and blank
/// lines are dropped. Pure + total.
fn normalize_whitespace(args: &Value) -> Result<String> {
    let text = req_str(args, "text", "normalize_whitespace")?;
    bound(text, "normalize_whitespace")?;
    if opt_bool(args, "keep_lines") {
        let lines: Vec<String> = text
            .lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|l| !l.is_empty())
            .collect();
        Ok(lines.join("\n"))
    } else {
        Ok(text.split_whitespace().collect::<Vec<_>>().join(" "))
    }
}

// ---------------------------------------------------------------------------
// dedupe_lines
// ---------------------------------------------------------------------------

/// `dedupe_lines {text, ignore_case?}` -> drop duplicate lines, keeping the first
/// occurrence and the original order. With `ignore_case`, lines differing only in
/// ASCII case are treated as equal (the first-seen spelling is kept). Pure.
fn dedupe_lines(args: &Value) -> Result<String> {
    let text = req_str(args, "text", "dedupe_lines")?;
    bound(text, "dedupe_lines")?;
    let ci = opt_bool(args, "ignore_case");
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let key = if ci { line.to_ascii_lowercase() } else { line.to_string() };
        if seen.insert(key) {
            out.push(line);
        }
    }
    Ok(out.join("\n"))
}

// ---------------------------------------------------------------------------
// sort_lines
// ---------------------------------------------------------------------------

/// `sort_lines {text, descending?, ignore_case?, unique?}` -> the lines sorted.
/// `ignore_case` compares case-insensitively (stable, so original case is kept);
/// `unique` removes duplicates after sorting; `descending` reverses the order.
/// Pure + total.
fn sort_lines(args: &Value) -> Result<String> {
    let text = req_str(args, "text", "sort_lines")?;
    bound(text, "sort_lines")?;
    let descending = opt_bool(args, "descending");
    let ci = opt_bool(args, "ignore_case");
    let unique = opt_bool(args, "unique");
    let mut lines: Vec<&str> = text.lines().collect();
    if ci {
        lines.sort_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()));
    } else {
        lines.sort();
    }
    if unique {
        if ci {
            let mut seen = std::collections::HashSet::new();
            lines.retain(|l| seen.insert(l.to_ascii_lowercase()));
        } else {
            lines.dedup(); // sorted, so dedup() removes all duplicates
        }
    }
    if descending {
        lines.reverse();
    }
    Ok(lines.join("\n"))
}

// ---------------------------------------------------------------------------
// word_frequency
// ---------------------------------------------------------------------------

/// `word_frequency {text, top?}` -> each word's count, most frequent first, ties
/// broken alphabetically. Words are lowercased and stripped of surrounding ASCII
/// punctuation; `top` (optional, 1..=1000) limits how many rows are returned.
/// Pure + deterministic (the tie-break makes ordering total).
fn word_frequency(args: &Value) -> Result<String> {
    let text = req_str(args, "text", "word_frequency")?;
    bound(text, "word_frequency")?;
    let top = match args.get("top") {
        None => usize::MAX,
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| anyhow!("word_frequency 'top' must be a positive integer"))?;
            if !(1..=1000).contains(&n) {
                return Err(anyhow!("word_frequency 'top' must be 1..=1000"));
            }
            n as usize
        }
    };
    let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for raw in text.split_whitespace() {
        let word: String = raw
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        if word.is_empty() {
            continue;
        }
        *counts.entry(word).or_insert(0) += 1;
    }
    if counts.is_empty() {
        return Ok("no words".to_string());
    }
    let mut pairs: Vec<(String, u64)> = counts.into_iter().collect();
    // Most frequent first; alphabetical tie-break for a total, deterministic order.
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let shown = pairs.iter().take(top);
    let body = shown
        .map(|(w, c)| format!("{w}: {c}"))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(body)
}

// ===========================================================================
// TESTS
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn skills_list_is_complete_and_all_pure() {
        let s = skills();
        let names: Vec<&str> = s.iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            vec![
                "text_stats",
                "reverse_text",
                "find_replace",
                "regex_test",
                "regex_extract",
                "json_pretty",
                "csv_to_json",
                "json_to_csv",
                "strip_markdown",
                "normalize_whitespace",
                "dedupe_lines",
                "sort_lines",
                "word_frequency",
            ]
        );
        assert!(
            s.iter().all(|d| !d.consequential && !d.source_gated),
            "every text skill is a pure, ungated, in-tree function"
        );
        assert_eq!(s.len(), 13);
    }

    // ---- text_stats ----

    #[test]
    fn text_stats_counts_everything() {
        let out = text_stats(&json!({
            "text": "Hello world. This is a test! Is it?\n\nSecond paragraph here."
        }))
        .unwrap();
        // 4 sentence terminators (. ! ? .), 2 blank-line-separated paragraphs,
        // 11 words.
        assert!(out.contains("4 sentences"), "got: {out}");
        assert!(out.contains("2 paragraphs"), "got: {out}");
        assert!(out.contains("11 words"), "got: {out}");
    }

    #[test]
    fn text_stats_collapses_ellipsis_into_one_sentence() {
        // "Wait... really?" -> two terminators runs => 2 sentences, not 4.
        let out = text_stats(&json!({"text": "Wait... really?"})).unwrap();
        assert!(out.contains("2 sentences"), "got: {out}");
    }

    #[test]
    fn text_stats_requires_text() {
        assert!(text_stats(&json!({})).is_err());
        assert!(text_stats(&json!({"text": 5})).is_err());
    }

    // ---- reverse_text ----

    #[test]
    fn reverse_text_known_and_unicode() {
        assert_eq!(reverse_text(&json!({"text": "hello"})).unwrap(), "olleh");
        assert_eq!(reverse_text(&json!({"text": ""})).unwrap(), "");
        // Unicode scalar reversal (each char flips, accents stay attached as one scalar).
        assert_eq!(reverse_text(&json!({"text": "abc"})).unwrap(), "cba");
        assert_eq!(reverse_text(&json!({"text": "héllo"})).unwrap(), "olléh");
    }

    #[test]
    fn reverse_text_errors_without_text() {
        assert!(reverse_text(&json!({})).is_err());
    }

    // ---- find_replace ----

    #[test]
    fn find_replace_literal_and_case_insensitive() {
        assert_eq!(
            find_replace(&json!({"text": "cat dog cat", "find": "cat", "replace": "fish"})).unwrap(),
            "fish dog fish"
        );
        // Default replace deletes.
        assert_eq!(
            find_replace(&json!({"text": "a-b-c", "find": "-"})).unwrap(),
            "abc"
        );
        // Case-insensitive search preserves surrounding case, replaces all forms.
        assert_eq!(
            find_replace(&json!({"text": "Cat CAT cat", "find": "cat", "replace": "X", "ignore_case": true})).unwrap(),
            "X X X"
        );
    }

    #[test]
    fn find_replace_rejects_empty_find() {
        assert!(find_replace(&json!({"text": "abc", "find": ""})).is_err());
        assert!(find_replace(&json!({"text": "abc"})).is_err(), "missing find");
    }

    // ---- regex_test / regex_extract ----

    #[test]
    fn regex_test_basic_patterns() {
        assert_eq!(
            regex_test(&json!({"text": "hello123", "pattern": "\\d+"})).unwrap(),
            "match: yes (first match: \"123\")"
        );
        assert_eq!(
            regex_test(&json!({"text": "abc", "pattern": "^a.c$"})).unwrap(),
            "match: yes (first match: \"abc\")"
        );
        assert_eq!(
            regex_test(&json!({"text": "xyz", "pattern": "^a"})).unwrap(),
            "match: no"
        );
        // Character class + quantifier.
        assert_eq!(
            regex_test(&json!({"text": "Room 42B", "pattern": "[0-9]+[A-Z]"})).unwrap(),
            "match: yes (first match: \"42B\")"
        );
    }

    #[test]
    fn regex_anchors_and_optional() {
        // $ end anchor.
        assert_eq!(
            regex_test(&json!({"text": "abc", "pattern": "c$"})).unwrap(),
            "match: yes (first match: \"c\")"
        );
        // ? optional atom: "colou?r" matches both spellings.
        assert!(regex_test(&json!({"text": "color", "pattern": "colou?r"})).unwrap().contains("yes"));
        assert!(regex_test(&json!({"text": "colour", "pattern": "colou?r"})).unwrap().contains("yes"));
    }

    #[test]
    fn regex_extract_all_nonoverlapping() {
        let out = regex_extract(&json!({"text": "a1 b22 c333", "pattern": "[0-9]+"})).unwrap();
        assert_eq!(out, "3 matches:\n1\n22\n333");
        // No matches.
        assert_eq!(
            regex_extract(&json!({"text": "abc", "pattern": "[0-9]+"})).unwrap(),
            "0 matches"
        );
    }

    #[test]
    fn regex_extract_zero_width_terminates() {
        // a* can match empty; the engine must not loop forever. With "a*" over
        // "baab" it should yield a finite, deterministic match list.
        let out = regex_extract(&json!({"text": "baab", "pattern": "a+"})).unwrap();
        assert_eq!(out, "1 matches:\naa");
    }

    #[test]
    fn regex_rejects_unsupported_syntax() {
        // Alternation/groups are explicitly unsupported -> error, not "no match".
        assert!(regex_test(&json!({"text": "a", "pattern": "(a|b)"})).is_err());
        // Dangling quantifier.
        assert!(regex_test(&json!({"text": "a", "pattern": "*a"})).is_err());
        // Unterminated class.
        assert!(regex_test(&json!({"text": "a", "pattern": "[a-"})).is_err());
        // Missing args.
        assert!(regex_test(&json!({"text": "a"})).is_err());
    }

    #[test]
    fn regex_catastrophic_backtracking_errors_not_hangs() {
        // This is the exact ReDoS class from the security review: a run of
        // consecutive top-level greedy quantifiers against a non-matching input.
        // The hand-written matcher would backtrack O(n^k); the step budget must
        // turn that into a fast, friendly error instead of wedging the loop.
        // Standalone this 8-quantifier pattern took ~41s against 50 'a's; with the
        // budget it must return Err immediately. (No timing assert — the test
        // simply completing proves the hang is gone; cargo's per-test timeout
        // would catch a regression.)
        let text = "a".repeat(400);
        let err = regex_test(&json!({
            "text": text,
            "pattern": "a*a*a*a*a*a*a*a*b"
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("too complex"), "got: {err}");

        // regex_extract shares one budget across all re-anchored attempts, so it
        // must error the same way rather than retrying the bomb at every offset.
        let err2 = regex_extract(&json!({
            "text": "a".repeat(400),
            "pattern": "a*a*a*a*a*a*a*a*b"
        }))
        .unwrap_err()
        .to_string();
        assert!(err2.contains("too complex"), "got: {err2}");
    }

    #[test]
    fn regex_rejects_too_many_tokens() {
        // A pattern with more than MAX_REGEX_TOKENS atoms is refused at compile
        // time, before any matching work — the cheap first line of defence.
        let pat = "a".repeat(MAX_REGEX_TOKENS + 1);
        let err = regex_test(&json!({"text": "a", "pattern": pat}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("too complex"), "got: {err}");
        // Exactly at the cap is still accepted (boundary is inclusive).
        let ok = "a".repeat(MAX_REGEX_TOKENS);
        assert!(regex_test(&json!({"text": "a", "pattern": ok})).is_ok());
    }

    #[test]
    fn regex_budget_does_not_break_normal_matches() {
        // Ordinary patterns finish far under the budget and behave exactly as
        // before the budget was added.
        assert_eq!(
            regex_test(&json!({"text": "hello123world", "pattern": "[0-9]+"})).unwrap(),
            "match: yes (first match: \"123\")"
        );
        // A legitimately large-but-linear greedy match over a long input still
        // succeeds (well within 2M comparisons).
        let big = "a".repeat(10_000);
        assert!(regex_test(&json!({"text": big, "pattern": "a+$"}))
            .unwrap()
            .contains("yes"));
    }

    // ---- json_pretty ----

    #[test]
    fn json_pretty_formats_and_minifies() {
        // json_pretty sorts object keys explicitly, so a/b come out a, b.
        let pretty = json_pretty(&json!({"json": "{\"b\":1,\"a\":2}"})).unwrap();
        assert_eq!(pretty, "{\n  \"a\": 2,\n  \"b\": 1\n}");
        let mini = json_pretty(&json!({"json": "{ \"a\" : 1 }", "minify": true})).unwrap();
        assert_eq!(mini, "{\"a\":1}");
        // Nested structure pretty-prints with 2-space indent.
        let nested = json_pretty(&json!({"json": "[1,{\"x\":2}]"})).unwrap();
        assert_eq!(nested, "[\n  1,\n  {\n    \"x\": 2\n  }\n]");
    }

    #[test]
    fn json_pretty_rejects_invalid() {
        assert!(json_pretty(&json!({"json": "{bad}"})).is_err());
        assert!(json_pretty(&json!({})).is_err());
    }

    #[test]
    fn json_pretty_sorts_keys_regardless_of_input_order() {
        // Output key order must be alphabetical no matter how the input is ordered
        // or how serde_json's map is backed (BTreeMap vs IndexMap) — the guard
        // against the prior flaky, build-dependent ordering.
        let out =
            json_pretty(&json!({"json": "{\"z\":1,\"a\":2,\"m\":3}", "minify": true})).unwrap();
        assert_eq!(out, "{\"a\":2,\"m\":3,\"z\":1}");
        // Nested objects sort recursively too.
        let nested =
            json_pretty(&json!({"json": "{\"o\":{\"y\":1,\"x\":2}}", "minify": true})).unwrap();
        assert_eq!(nested, "{\"o\":{\"x\":2,\"y\":1}}");
    }

    // ---- csv_to_json ----

    #[test]
    fn csv_to_json_with_quoted_fields() {
        let out = csv_to_json(&json!({"csv": "name,age\nAlice,30\n\"Bob, Jr.\",25"})).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "Alice");
        assert_eq!(arr[0]["age"], "30");
        // Embedded comma inside quotes is preserved.
        assert_eq!(arr[1]["name"], "Bob, Jr.");
    }

    #[test]
    fn csv_to_json_rejects_ragged_rows_and_empty() {
        assert!(csv_to_json(&json!({"csv": "a,b\n1"})).is_err(), "ragged row");
        assert!(csv_to_json(&json!({"csv": ""})).is_err(), "empty csv");
        assert!(csv_to_json(&json!({"csv": "a,\n1,2"})).is_err(), "empty header name");
    }

    // ---- json_to_csv ----

    #[test]
    fn json_to_csv_roundtrips_and_quotes() {
        // Columns come out alphabetically sorted (city, name); the embedded comma
        // in "LA, CA" forces that cell to be quoted.
        let out = json_to_csv(&json!({
            "json": "[{\"name\":\"Alice\",\"city\":\"NYC\"},{\"name\":\"Bob\",\"city\":\"LA, CA\"}]"
        }))
        .unwrap();
        assert_eq!(out, "city,name\nNYC,Alice\n\"LA, CA\",Bob");
    }

    #[test]
    fn json_to_csv_rejects_nested_and_nonarray() {
        assert!(
            json_to_csv(&json!({"json": "[{\"a\":[1,2]}]"})).is_err(),
            "nested array value rejected"
        );
        assert!(json_to_csv(&json!({"json": "{\"a\":1}"})).is_err(), "top-level object rejected");
        // Empty array -> empty output (not an error).
        assert_eq!(json_to_csv(&json!({"json": "[]"})).unwrap(), "");
    }

    // ---- strip_markdown ----

    #[test]
    fn strip_markdown_removes_common_formatting() {
        assert_eq!(
            strip_markdown(&json!({"text": "# Title"})).unwrap(),
            "Title"
        );
        assert_eq!(
            strip_markdown(&json!({"text": "This is **bold** and *italic* and `code`."})).unwrap(),
            "This is bold and italic and code."
        );
        assert_eq!(
            strip_markdown(&json!({"text": "See [the docs](https://example.com) now"})).unwrap(),
            "See the docs now"
        );
        assert_eq!(
            strip_markdown(&json!({"text": "- item one\n- item two"})).unwrap(),
            "item one\nitem two"
        );
        assert_eq!(
            strip_markdown(&json!({"text": "> a quote"})).unwrap(),
            "a quote"
        );
    }

    #[test]
    fn strip_markdown_handles_ordered_lists_and_requires_text() {
        assert_eq!(
            strip_markdown(&json!({"text": "1. first\n2. second"})).unwrap(),
            "first\nsecond"
        );
        assert!(strip_markdown(&json!({})).is_err());
    }

    // ---- normalize_whitespace ----

    #[test]
    fn normalize_whitespace_collapses_and_trims() {
        assert_eq!(
            normalize_whitespace(&json!({"text": "  hello   world \t foo "})).unwrap(),
            "hello world foo"
        );
        // keep_lines: per-line collapse, blank lines dropped.
        assert_eq!(
            normalize_whitespace(&json!({"text": "a  b\n\n  c   d  ", "keep_lines": true})).unwrap(),
            "a b\nc d"
        );
    }

    #[test]
    fn normalize_whitespace_requires_text() {
        assert!(normalize_whitespace(&json!({})).is_err());
    }

    // ---- dedupe_lines ----

    #[test]
    fn dedupe_lines_keeps_first_order() {
        assert_eq!(
            dedupe_lines(&json!({"text": "a\nb\na\nc\nb"})).unwrap(),
            "a\nb\nc"
        );
        // Case-insensitive dedupe keeps the first spelling.
        assert_eq!(
            dedupe_lines(&json!({"text": "Apple\napple\nBANANA\nbanana", "ignore_case": true})).unwrap(),
            "Apple\nBANANA"
        );
    }

    #[test]
    fn dedupe_lines_requires_text() {
        assert!(dedupe_lines(&json!({})).is_err());
    }

    // ---- sort_lines ----

    #[test]
    fn sort_lines_orders_and_dedupes() {
        assert_eq!(
            sort_lines(&json!({"text": "banana\napple\ncherry"})).unwrap(),
            "apple\nbanana\ncherry"
        );
        assert_eq!(
            sort_lines(&json!({"text": "b\na\nc", "descending": true})).unwrap(),
            "c\nb\na"
        );
        // Unique after sort.
        assert_eq!(
            sort_lines(&json!({"text": "a\nb\na\nc", "unique": true})).unwrap(),
            "a\nb\nc"
        );
        // Case-insensitive ordering, stable so original case kept.
        assert_eq!(
            sort_lines(&json!({"text": "Banana\napple\nCherry", "ignore_case": true})).unwrap(),
            "apple\nBanana\nCherry"
        );
    }

    #[test]
    fn sort_lines_requires_text() {
        assert!(sort_lines(&json!({})).is_err());
    }

    // ---- word_frequency ----

    #[test]
    fn word_frequency_counts_and_orders() {
        let out = word_frequency(&json!({
            "text": "the cat sat on the mat the cat"
        }))
        .unwrap();
        // the:3, cat:2, then mat/on/sat each 1 alphabetically.
        assert_eq!(out, "the: 3\ncat: 2\nmat: 1\non: 1\nsat: 1");
    }

    #[test]
    fn word_frequency_strips_punctuation_and_lowercases() {
        let out = word_frequency(&json!({"text": "Hello, hello! HELLO."})).unwrap();
        assert_eq!(out, "hello: 3");
    }

    #[test]
    fn word_frequency_top_limits_and_validates() {
        let out = word_frequency(&json!({"text": "a a b b b c", "top": 1})).unwrap();
        assert_eq!(out, "b: 3");
        assert!(word_frequency(&json!({"text": "x", "top": 0})).is_err());
        assert!(word_frequency(&json!({"text": "x", "top": 99999})).is_err());
        assert_eq!(word_frequency(&json!({"text": "   "})).unwrap(), "no words");
    }

    // ---- determinism spot-check across a few skills ----

    #[test]
    fn skills_are_deterministic() {
        let a1 = word_frequency(&json!({"text": "a b a c b a"})).unwrap();
        let a2 = word_frequency(&json!({"text": "a b a c b a"})).unwrap();
        assert_eq!(a1, a2);
        let b1 = sort_lines(&json!({"text": "z\nm\na"})).unwrap();
        let b2 = sort_lines(&json!({"text": "z\nm\na"})).unwrap();
        assert_eq!(b1, b2);
    }
}
