//! Category: FUN — playful helpers that stay DETERMINISTIC-WITH-A-SEED: coin
//! flip, random pick from a list, magic-8-ball, and the deterministic word games
//! (palindrome/anagram/wordle, pig-latin, leetspeak, ASCII banner). Every skill
//! that would otherwise be "random" takes a REQUIRED `seed` argument so its
//! result is reproducible and hermetically testable — there is no ambient entropy
//! and no clock. (The proof `dice_roll` lives in `utilities`; this category is for
//! the rest of the playful set.)
//!
//! Every `run` here is a PURE, total function of its args: no network, no clock,
//! no I/O, no un-seeded randomness. Bad input yields a friendly error — never a
//! panic, never a fabricated value.

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{Category, SkillDef};

/// The fun catalog. The Library phase appends `SkillDef::new(...)` entries to THIS
/// vec (and nothing in mod.rs changes).
pub fn skills() -> Vec<SkillDef> {
    vec![
        SkillDef::new(
            "coin_flip",
            Category::Fun,
            "Flip one or more coins deterministically from a seed (heads/tails). Use for a coin toss; REQUIRES a seed so the result is reproducible — there is no ambient randomness.",
            &["flip a coin", "coin toss", "heads or tails", "coin flip"],
            coin_flip,
        ),
        SkillDef::new(
            "magic_8_ball",
            Category::Fun,
            "Ask the Magic 8-Ball a yes/no question and get one of the 20 classic answers, chosen deterministically from a seed. Use for a playful fortune; REQUIRES a seed.",
            &["magic 8 ball", "magic eight ball", "ask the 8 ball", "fortune"],
            magic_8_ball,
        ),
        SkillDef::new(
            "rock_paper_scissors",
            Category::Fun,
            "Play one round of rock-paper-scissors: given the player's move, pick the house move from a seed and report the winner. Use for a quick RPS game; REQUIRES a seed.",
            &["rock paper scissors", "rps", "roshambo", "play rock paper scissors"],
            rock_paper_scissors,
        ),
        SkillDef::new(
            "random_pick",
            Category::Fun,
            "Pick one item from a list deterministically from a seed. Use to choose among options (what to eat, who goes first); REQUIRES a seed so the pick is reproducible.",
            &["pick one", "choose for me", "random choice", "pick from list", "decide for me"],
            random_pick,
        ),
        SkillDef::new(
            "palindrome_check",
            Category::Fun,
            "Check whether text reads the same forwards and backwards (ignoring case, spaces, and punctuation). Use when the user asks if a word or phrase is a palindrome.",
            &["is this a palindrome", "palindrome", "reads the same backwards"],
            palindrome_check,
        ),
        SkillDef::new(
            "anagram_check",
            Category::Fun,
            "Check whether two strings are anagrams (same letters, ignoring case, spaces, and punctuation). Use when the user asks if two words/phrases are anagrams of each other.",
            &["are these anagrams", "anagram", "same letters rearranged"],
            anagram_check,
        ),
        SkillDef::new(
            "wordle_feedback",
            Category::Fun,
            "Score a 5-letter Wordle guess against the answer, returning the green/yellow/gray pattern with correct duplicate-letter handling. Use to evaluate a Wordle guess.",
            &["wordle", "wordle feedback", "score my guess", "green yellow gray"],
            wordle_feedback,
        ),
        SkillDef::new(
            "pig_latin",
            Category::Fun,
            "Translate text into Pig Latin (consonant clusters moved to the end + 'ay'; vowel-initial words get 'way'). Use when the user wants Pig Latin.",
            &["pig latin", "translate to pig latin", "igpay atinlay"],
            pig_latin,
        ),
        SkillDef::new(
            "leetspeak",
            Category::Fun,
            "Convert text to or from basic leetspeak (a->4, e->3, l->1, o->0, s->5, t->7). Use when the user wants l33t / 1337 text, or to decode it.",
            &["leetspeak", "l33t", "1337", "leet speak", "decode leet"],
            leetspeak,
        ),
        SkillDef::new(
            "ascii_banner",
            Category::Fun,
            "Render short A-Z/0-9/space text as a 5-row ASCII-art banner from a bundled block font. Use for a small ASCII header/banner. Bounded to keep output readable.",
            &["ascii banner", "ascii art text", "big letters", "make a banner"],
            ascii_banner,
        ),
        SkillDef::new(
            "shuffle_list",
            Category::Fun,
            "Shuffle a list into a deterministic order from a seed (Fisher-Yates). Use to randomize an order (teams, playlist, draw order); REQUIRES a seed so it is reproducible.",
            &["shuffle", "randomize order", "shuffle the list", "draw order"],
            shuffle_list,
        ),
    ]
}

// ---------------------------------------------------------------------------
// Deterministic PRNG (shared). SplitMix64 — a tiny, well-known, fully
// reproducible step. Seeded, so every "random" skill is hermetic: no OS
// entropy, no clock. Identical to the one `utilities::dice_roll` uses.
// ---------------------------------------------------------------------------

/// Advance `state` and return a scrambled 64-bit value. Pure + deterministic.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Read a REQUIRED integer `seed` arg, with a friendly error if it is missing or
/// not an unsigned integer. Centralised so every seeded skill reports the same way.
fn read_seed(args: &Value, skill: &str) -> Result<u64> {
    args.get("seed").and_then(Value::as_u64).ok_or_else(|| {
        anyhow!("{skill} needs an integer 'seed' (so the result is reproducible)")
    })
}

// ---------------------------------------------------------------------------
// coin_flip
// ---------------------------------------------------------------------------

/// `coin_flip {seed, count?}` -> deterministic heads/tails. `count` (default 1)
/// independent flips driven by the REQUIRED `seed`. Pure.
fn coin_flip(args: &Value) -> Result<String> {
    let seed = read_seed(args, "coin_flip")?;
    let count = args.get("count").and_then(Value::as_u64).unwrap_or(1);
    if !(1..=100).contains(&count) {
        return Err(anyhow!("coin_flip 'count' must be 1..=100"));
    }
    let mut state = seed;
    let mut flips = Vec::with_capacity(count as usize);
    let mut heads = 0u64;
    for _ in 0..count {
        if splitmix64(&mut state) & 1 == 0 {
            heads += 1;
            flips.push("heads");
        } else {
            flips.push("tails");
        }
    }
    if count == 1 {
        Ok(format!("{} (seed {seed})", flips[0]))
    } else {
        Ok(format!(
            "{} (seed {seed}): {heads} heads, {} tails",
            flips.join(", "),
            count - heads
        ))
    }
}

// ---------------------------------------------------------------------------
// magic_8_ball
// ---------------------------------------------------------------------------

/// The 20 canonical Magic 8-Ball answers, in their classic order.
const EIGHT_BALL: [&str; 20] = [
    "It is certain.",
    "It is decidedly so.",
    "Without a doubt.",
    "Yes definitely.",
    "You may rely on it.",
    "As I see it, yes.",
    "Most likely.",
    "Outlook good.",
    "Yes.",
    "Signs point to yes.",
    "Reply hazy, try again.",
    "Ask again later.",
    "Better not tell you now.",
    "Cannot predict now.",
    "Concentrate and ask again.",
    "Don't count on it.",
    "My reply is no.",
    "My sources say no.",
    "Outlook not so good.",
    "Very doubtful.",
];

/// `magic_8_ball {seed, question?}` -> one of the 20 classic answers, chosen by
/// the REQUIRED `seed`. The optional `question` is echoed back. Pure.
fn magic_8_ball(args: &Value) -> Result<String> {
    let seed = read_seed(args, "magic_8_ball")?;
    let mut state = seed;
    let idx = (splitmix64(&mut state) % EIGHT_BALL.len() as u64) as usize;
    let answer = EIGHT_BALL[idx];
    match args.get("question").and_then(Value::as_str) {
        Some(q) if !q.trim().is_empty() => Ok(format!("\u{1f3b1} \"{}\" -> {answer}", q.trim())),
        _ => Ok(format!("\u{1f3b1} {answer}")),
    }
}

// ---------------------------------------------------------------------------
// rock_paper_scissors
// ---------------------------------------------------------------------------

/// Normalise a move string to "rock" | "paper" | "scissors", accepting common
/// shorthands. `None` for anything unrecognised.
fn rps_normalize(s: &str) -> Option<&'static str> {
    match s.trim().to_ascii_lowercase().as_str() {
        "rock" | "r" => Some("rock"),
        "paper" | "p" => Some("paper"),
        "scissors" | "scissor" | "s" => Some("scissors"),
        _ => None,
    }
}

/// `rock_paper_scissors {move, seed}` -> the house move (from the seed) and the
/// outcome from the player's perspective. Pure + deterministic.
fn rock_paper_scissors(args: &Value) -> Result<String> {
    let player_raw = args
        .get("move")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("rock_paper_scissors needs a 'move' (rock, paper, or scissors)"))?;
    let player = rps_normalize(player_raw)
        .ok_or_else(|| anyhow!("rock_paper_scissors 'move' must be rock, paper, or scissors"))?;
    let seed = read_seed(args, "rock_paper_scissors")?;
    let moves = ["rock", "paper", "scissors"];
    let mut state = seed;
    let house = moves[(splitmix64(&mut state) % 3) as usize];
    let outcome = if player == house {
        "tie"
    } else if beats(player, house) {
        "you win"
    } else {
        "you lose"
    };
    Ok(format!("you: {player} vs house: {house} -> {outcome} (seed {seed})"))
}

/// Does move `a` beat move `b` in rock-paper-scissors? Pure.
fn beats(a: &str, b: &str) -> bool {
    matches!(
        (a, b),
        ("rock", "scissors") | ("paper", "rock") | ("scissors", "paper")
    )
}

// ---------------------------------------------------------------------------
// random_pick
// ---------------------------------------------------------------------------

/// `random_pick {items, seed}` -> one element chosen by the REQUIRED `seed`.
/// `items` is a non-empty JSON array of strings. Pure + deterministic.
fn random_pick(args: &Value) -> Result<String> {
    let items = args
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("random_pick needs an 'items' array of strings"))?;
    if items.is_empty() {
        return Err(anyhow!("random_pick 'items' must be a non-empty array"));
    }
    if items.len() > 1000 {
        return Err(anyhow!("random_pick 'items' must have at most 1000 entries"));
    }
    let strs: Vec<&str> = items
        .iter()
        .map(|v| v.as_str())
        .collect::<Option<Vec<&str>>>()
        .ok_or_else(|| anyhow!("random_pick 'items' must all be strings"))?;
    let seed = read_seed(args, "random_pick")?;
    let mut state = seed;
    let idx = (splitmix64(&mut state) % strs.len() as u64) as usize;
    Ok(format!("{} (seed {seed})", strs[idx]))
}

// ---------------------------------------------------------------------------
// shuffle_list
// ---------------------------------------------------------------------------

/// `shuffle_list {items, seed}` -> the items reordered by a seeded Fisher-Yates
/// shuffle. A permutation of the input (no loss, no duplication). Pure.
fn shuffle_list(args: &Value) -> Result<String> {
    let items = args
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("shuffle_list needs an 'items' array of strings"))?;
    if items.is_empty() {
        return Err(anyhow!("shuffle_list 'items' must be a non-empty array"));
    }
    if items.len() > 1000 {
        return Err(anyhow!("shuffle_list 'items' must have at most 1000 entries"));
    }
    let mut strs: Vec<String> = items
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect::<Option<Vec<String>>>()
        .ok_or_else(|| anyhow!("shuffle_list 'items' must all be strings"))?;
    let seed = read_seed(args, "shuffle_list")?;
    let mut state = seed;
    // Fisher-Yates: walk from the end, swapping each element with a uniformly
    // chosen earlier-or-equal index. Modulo (i+1) is fine here (the seed space is
    // 64-bit; this is a game shuffle, not a cryptographic one).
    for i in (1..strs.len()).rev() {
        let j = (splitmix64(&mut state) % (i as u64 + 1)) as usize;
        strs.swap(i, j);
    }
    Ok(format!("{} (seed {seed})", strs.join(", ")))
}

// ---------------------------------------------------------------------------
// palindrome_check
// ---------------------------------------------------------------------------

/// Lowercase the alphanumeric scalar values of `s`, dropping everything else.
/// Shared by palindrome + anagram so both ignore case/spaces/punctuation alike.
fn alnum_lower(s: &str) -> Vec<char> {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// `palindrome_check {text}` -> whether `text` reads the same both ways, ignoring
/// case, spaces, and punctuation. Pure + total.
fn palindrome_check(args: &Value) -> Result<String> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("palindrome_check needs a 'text' string argument"))?;
    let cleaned = alnum_lower(text);
    if cleaned.is_empty() {
        return Err(anyhow!(
            "palindrome_check needs at least one letter or digit in 'text'"
        ));
    }
    let is_pal = cleaned.iter().eq(cleaned.iter().rev());
    if is_pal {
        Ok(format!("\"{text}\" is a palindrome."))
    } else {
        Ok(format!("\"{text}\" is not a palindrome."))
    }
}

// ---------------------------------------------------------------------------
// anagram_check
// ---------------------------------------------------------------------------

/// `anagram_check {a, b}` -> whether the two strings are anagrams, ignoring case,
/// spaces, and punctuation. Pure + total.
fn anagram_check(args: &Value) -> Result<String> {
    let a = args
        .get("a")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("anagram_check needs string arguments 'a' and 'b'"))?;
    let b = args
        .get("b")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("anagram_check needs string arguments 'a' and 'b'"))?;
    let mut ca = alnum_lower(a);
    let mut cb = alnum_lower(b);
    if ca.is_empty() || cb.is_empty() {
        return Err(anyhow!(
            "anagram_check needs at least one letter or digit in each of 'a' and 'b'"
        ));
    }
    ca.sort_unstable();
    cb.sort_unstable();
    if ca == cb {
        Ok(format!("\"{a}\" and \"{b}\" are anagrams."))
    } else {
        Ok(format!("\"{a}\" and \"{b}\" are not anagrams."))
    }
}

// ---------------------------------------------------------------------------
// wordle_feedback
// ---------------------------------------------------------------------------

/// `wordle_feedback {guess, answer}` -> the green/yellow/gray pattern for a
/// 5-letter guess against the answer, with correct duplicate-letter handling (a
/// guessed letter only earns yellow while unmatched copies of it remain in the
/// answer). Returns a glyph row plus a letter-coded row (G/Y/-). Pure + total.
fn wordle_feedback(args: &Value) -> Result<String> {
    let guess = args
        .get("guess")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("wordle_feedback needs string arguments 'guess' and 'answer'"))?;
    let answer = args
        .get("answer")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("wordle_feedback needs string arguments 'guess' and 'answer'"))?;

    let g: Vec<char> = guess.trim().to_ascii_lowercase().chars().collect();
    let a: Vec<char> = answer.trim().to_ascii_lowercase().chars().collect();
    if g.len() != 5 || a.len() != 5 {
        return Err(anyhow!("wordle_feedback needs a 5-letter 'guess' and 5-letter 'answer'"));
    }
    if !g.iter().all(|c| c.is_ascii_lowercase()) || !a.iter().all(|c| c.is_ascii_lowercase()) {
        return Err(anyhow!("wordle_feedback 'guess' and 'answer' must be A-Z letters only"));
    }

    // Two-pass scoring. Pass 1: greens, consuming the matched answer letters from
    // a tally. Pass 2: yellows only while an unmatched copy remains; else gray.
    let mut marks = ['-'; 5];
    let mut counts = [0u8; 26];
    for (i, &ac) in a.iter().enumerate() {
        if g[i] == ac {
            marks[i] = 'G';
        } else {
            counts[(ac as u8 - b'a') as usize] += 1;
        }
    }
    for i in 0..5 {
        if marks[i] == 'G' {
            continue;
        }
        let slot = (g[i] as u8 - b'a') as usize;
        if counts[slot] > 0 {
            marks[i] = 'Y';
            counts[slot] -= 1;
        }
    }

    let glyphs: String = marks
        .iter()
        .map(|m| match m {
            'G' => '\u{1f7e9}', // green square
            'Y' => '\u{1f7e8}', // yellow square
            _ => '\u{2b1b}',    // black square
        })
        .collect();
    let coded: String = marks.iter().collect();
    Ok(format!(
        "{} ({}) {glyphs} [{coded}]",
        guess.trim().to_ascii_uppercase(),
        answer.trim().to_ascii_uppercase()
    ))
}

// ---------------------------------------------------------------------------
// pig_latin
// ---------------------------------------------------------------------------

/// Is `c` a vowel for Pig Latin purposes (a/e/i/o/u; 'y' is treated as a
/// consonant when leading, the common schoolyard rule).
fn is_vowel(c: char) -> bool {
    matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u')
}

/// Convert a single alphabetic word to Pig Latin, preserving capitalisation of
/// the first letter. Vowel-initial -> "+way"; otherwise move the leading
/// consonant cluster to the end and add "ay".
fn pig_latin_word(word: &str) -> String {
    let chars: Vec<char> = word.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    let cap = chars[0].is_uppercase();
    let lower: Vec<char> = word.chars().flat_map(|c| c.to_lowercase()).collect();

    let piggy: String = if is_vowel(lower[0]) {
        format!("{}way", lower.iter().collect::<String>())
    } else {
        // Find the first vowel; everything before it is the onset cluster.
        let split = lower.iter().position(|&c| is_vowel(c)).unwrap_or(lower.len());
        let (onset, rest) = lower.split_at(split);
        format!(
            "{}{}ay",
            rest.iter().collect::<String>(),
            onset.iter().collect::<String>()
        )
    };

    if cap {
        let mut cs = piggy.chars();
        match cs.next() {
            Some(first) => first.to_uppercase().collect::<String>() + cs.as_str(),
            None => piggy,
        }
    } else {
        piggy
    }
}

/// `pig_latin {text}` -> the text translated to Pig Latin. Alphabetic words are
/// translated; non-alphabetic runs (spaces, punctuation, digits) pass through
/// unchanged so spacing/punctuation is preserved. Pure + total.
fn pig_latin(args: &Value) -> Result<String> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("pig_latin needs a 'text' string argument"))?;
    let mut out = String::with_capacity(text.len() + 8);
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if !word.is_empty() {
            out.push_str(&pig_latin_word(word));
            word.clear();
        }
    };
    for c in text.chars() {
        if c.is_alphabetic() {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    Ok(out)
}

// ---------------------------------------------------------------------------
// leetspeak
// ---------------------------------------------------------------------------

/// `leetspeak {text, decode?}` -> basic leetspeak. Encoding maps
/// a->4, e->3, l->1, o->0, s->5, t->7 (other characters unchanged). With
/// `decode: true` the inverse mapping is applied (4->a, 3->e, 1->l, 0->o, 5->s,
/// 7->t). Pure + total.
fn leetspeak(args: &Value) -> Result<String> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("leetspeak needs a 'text' string argument"))?;
    let decode = args.get("decode").and_then(Value::as_bool).unwrap_or(false);
    let out: String = text
        .chars()
        .map(|c| {
            if decode {
                match c {
                    '4' => 'a',
                    '3' => 'e',
                    '1' => 'l',
                    '0' => 'o',
                    '5' => 's',
                    '7' => 't',
                    other => other,
                }
            } else {
                match c.to_ascii_lowercase() {
                    'a' => '4',
                    'e' => '3',
                    'l' => '1',
                    'o' => '0',
                    's' => '5',
                    't' => '7',
                    _ => c,
                }
            }
        })
        .collect();
    Ok(out)
}

// ---------------------------------------------------------------------------
// ascii_banner
// ---------------------------------------------------------------------------

/// Five rows of a tiny block font for one glyph (A-Z, 0-9, space). Each glyph is
/// 5 chars wide × 5 rows. Hand-drawn so the skill carries no font dependency.
fn glyph_rows(c: char) -> [&'static str; 5] {
    match c.to_ascii_uppercase() {
        'A' => [" ### ", "#   #", "#####", "#   #", "#   #"],
        'B' => ["#### ", "#   #", "#### ", "#   #", "#### "],
        'C' => [" ####", "#    ", "#    ", "#    ", " ####"],
        'D' => ["#### ", "#   #", "#   #", "#   #", "#### "],
        'E' => ["#####", "#    ", "#### ", "#    ", "#####"],
        'F' => ["#####", "#    ", "#### ", "#    ", "#    "],
        'G' => [" ####", "#    ", "#  ##", "#   #", " ####"],
        'H' => ["#   #", "#   #", "#####", "#   #", "#   #"],
        'I' => ["#####", "  #  ", "  #  ", "  #  ", "#####"],
        'J' => ["#####", "   # ", "   # ", "#  # ", " ##  "],
        'K' => ["#   #", "#  # ", "###  ", "#  # ", "#   #"],
        'L' => ["#    ", "#    ", "#    ", "#    ", "#####"],
        'M' => ["#   #", "## ##", "# # #", "#   #", "#   #"],
        'N' => ["#   #", "##  #", "# # #", "#  ##", "#   #"],
        'O' => [" ### ", "#   #", "#   #", "#   #", " ### "],
        'P' => ["#### ", "#   #", "#### ", "#    ", "#    "],
        'Q' => [" ### ", "#   #", "# # #", "#  # ", " ## #"],
        'R' => ["#### ", "#   #", "#### ", "#  # ", "#   #"],
        'S' => [" ####", "#    ", " ### ", "    #", "#### "],
        'T' => ["#####", "  #  ", "  #  ", "  #  ", "  #  "],
        'U' => ["#   #", "#   #", "#   #", "#   #", " ### "],
        'V' => ["#   #", "#   #", "#   #", " # # ", "  #  "],
        'W' => ["#   #", "#   #", "# # #", "## ##", "#   #"],
        'X' => ["#   #", " # # ", "  #  ", " # # ", "#   #"],
        'Y' => ["#   #", " # # ", "  #  ", "  #  ", "  #  "],
        'Z' => ["#####", "   # ", "  #  ", " #   ", "#####"],
        '0' => [" ### ", "#  ##", "# # #", "##  #", " ### "],
        '1' => ["  #  ", " ##  ", "  #  ", "  #  ", "#####"],
        '2' => [" ### ", "#   #", "  ## ", " #   ", "#####"],
        '3' => ["#### ", "    #", " ### ", "    #", "#### "],
        '4' => ["#   #", "#   #", "#####", "    #", "    #"],
        '5' => ["#####", "#    ", "#### ", "    #", "#### "],
        '6' => [" ### ", "#    ", "#### ", "#   #", " ### "],
        '7' => ["#####", "    #", "   # ", "  #  ", "  #  "],
        '8' => [" ### ", "#   #", " ### ", "#   #", " ### "],
        '9' => [" ### ", "#   #", " ####", "    #", " ### "],
        _ => ["     ", "     ", "     ", "     ", "     "], // space / unsupported
    }
}

/// `ascii_banner {text}` -> a 5-row ASCII-art rendering of `text`. Supports A-Z,
/// 0-9, and space (case-insensitive; unsupported characters render as blank
/// columns). Bounded to 20 characters so the output stays readable. Pure + total.
fn ascii_banner(args: &Value) -> Result<String> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("ascii_banner needs a 'text' string argument"))?;
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Err(anyhow!("ascii_banner 'text' must not be empty"));
    }
    if chars.len() > 20 {
        return Err(anyhow!("ascii_banner 'text' must be at most 20 characters"));
    }
    // Validate the supported alphabet so the result is honest (no silent blanks
    // for, say, punctuation the caller expected to render).
    for &c in &chars {
        let ok = c == ' ' || c.is_ascii_alphanumeric();
        if !ok {
            return Err(anyhow!(
                "ascii_banner supports A-Z, 0-9, and spaces only (got '{c}')"
            ));
        }
    }
    let mut lines = [
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
    ];
    for (i, &c) in chars.iter().enumerate() {
        let rows = glyph_rows(c);
        for (row, line) in lines.iter_mut().enumerate() {
            if i > 0 {
                line.push(' '); // one-column gap between glyphs
            }
            line.push_str(rows[row]);
        }
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- coin_flip ---------------------------------------------------------

    #[test]
    fn coin_flip_is_reproducible_and_two_sided() {
        let a = coin_flip(&json!({"seed": 1})).unwrap();
        let b = coin_flip(&json!({"seed": 1})).unwrap();
        assert_eq!(a, b, "same seed -> same flip");
        assert!(a.starts_with("heads") || a.starts_with("tails"));
        assert!(a.ends_with("(seed 1)"));
        // Both faces are reachable across seeds (the PRNG isn't stuck on one).
        let outcomes: std::collections::HashSet<&str> = (0..40u64)
            .map(|s| {
                if coin_flip(&json!({ "seed": s })).unwrap().starts_with("heads") {
                    "h"
                } else {
                    "t"
                }
            })
            .collect();
        assert_eq!(outcomes.len(), 2, "both heads and tails occur");
    }

    #[test]
    fn coin_flip_counts_a_batch_and_validates() {
        let out = coin_flip(&json!({"seed": 9, "count": 10})).unwrap();
        // "h, t, ... (seed 9): X heads, Y tails" with X+Y == 10.
        let tail = out.split("(seed 9): ").nth(1).unwrap();
        let heads: u64 = tail.split(" heads").next().unwrap().parse().unwrap();
        let tails: u64 = tail
            .split(", ")
            .nth(1)
            .unwrap()
            .split(" tails")
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(heads + tails, 10, "heads + tails == count");
        assert!(coin_flip(&json!({})).is_err(), "missing seed");
        assert!(coin_flip(&json!({"seed": 1, "count": 0})).is_err(), "count 0");
        assert!(coin_flip(&json!({"seed": 1, "count": 101})).is_err(), "count too big");
    }

    // --- magic_8_ball ------------------------------------------------------

    #[test]
    fn magic_8_ball_returns_a_canonical_answer_deterministically() {
        let a = magic_8_ball(&json!({"seed": 3})).unwrap();
        let b = magic_8_ball(&json!({"seed": 3})).unwrap();
        assert_eq!(a, b, "same seed -> same answer");
        // The answer is one of the 20 canonical strings.
        assert!(
            EIGHT_BALL.iter().any(|ans| a.contains(ans)),
            "answer must be canonical: {a}"
        );
        // Question is echoed (trimmed) when provided.
        let q = magic_8_ball(&json!({"seed": 3, "question": "  will it rain?  "})).unwrap();
        assert!(q.contains("\"will it rain?\""), "echoes the question: {q}");
    }

    #[test]
    fn magic_8_ball_covers_its_answer_space_and_needs_a_seed() {
        // Across many seeds we should see a healthy spread, not a single stuck answer.
        let distinct: std::collections::HashSet<String> = (0..200u64)
            .map(|s| magic_8_ball(&json!({ "seed": s })).unwrap())
            .collect();
        assert!(distinct.len() >= 15, "covers most of the 20 answers, got {}", distinct.len());
        assert!(magic_8_ball(&json!({})).is_err(), "missing seed -> error");
    }

    // --- rock_paper_scissors ----------------------------------------------

    #[test]
    fn rps_rules_are_correct() {
        assert!(beats("rock", "scissors"));
        assert!(beats("paper", "rock"));
        assert!(beats("scissors", "paper"));
        assert!(!beats("rock", "paper"));
        assert!(!beats("scissors", "rock"));
        assert!(!beats("rock", "rock"));
    }

    #[test]
    fn rps_plays_a_deterministic_round_with_correct_outcome() {
        let out = rock_paper_scissors(&json!({"move": "rock", "seed": 5})).unwrap();
        let again = rock_paper_scissors(&json!({"move": "rock", "seed": 5})).unwrap();
        assert_eq!(out, again, "same move+seed -> same round");
        // Reconstruct the house move and verify the stated outcome is correct.
        let house = out
            .split("house: ")
            .nth(1)
            .unwrap()
            .split(" ->")
            .next()
            .unwrap();
        let expected = if "rock" == house {
            "tie"
        } else if beats("rock", house) {
            "you win"
        } else {
            "you lose"
        };
        assert!(out.contains(expected), "outcome must match the rules: {out}");
        // Shorthand + capitalised input normalises.
        assert!(rock_paper_scissors(&json!({"move": "P", "seed": 1})).unwrap().starts_with("you: paper"));
    }

    #[test]
    fn rps_rejects_bad_move_and_missing_seed() {
        assert!(rock_paper_scissors(&json!({"move": "lizard", "seed": 1})).is_err());
        assert!(rock_paper_scissors(&json!({"seed": 1})).is_err(), "missing move");
        assert!(rock_paper_scissors(&json!({"move": "rock"})).is_err(), "missing seed");
    }

    // --- random_pick -------------------------------------------------------

    #[test]
    fn random_pick_chooses_a_member_deterministically() {
        let items = json!({"items": ["tea", "coffee", "water"], "seed": 11});
        let a = random_pick(&items).unwrap();
        let b = random_pick(&items).unwrap();
        assert_eq!(a, b, "same items+seed -> same pick");
        let picked = a.split(" (seed").next().unwrap();
        assert!(["tea", "coffee", "water"].contains(&picked), "picks a real member: {picked}");
        // Single-element list always returns that element.
        assert!(random_pick(&json!({"items": ["only"], "seed": 99})).unwrap().starts_with("only"));
    }

    #[test]
    fn random_pick_validates_input() {
        assert!(random_pick(&json!({"items": [], "seed": 1})).is_err(), "empty list");
        assert!(random_pick(&json!({"seed": 1})).is_err(), "missing items");
        assert!(random_pick(&json!({"items": ["a"]})).is_err(), "missing seed");
        assert!(random_pick(&json!({"items": [1, 2, 3], "seed": 1})).is_err(), "non-string items");
    }

    // --- shuffle_list ------------------------------------------------------

    #[test]
    fn shuffle_list_is_a_deterministic_permutation() {
        let args = json!({"items": ["a", "b", "c", "d", "e"], "seed": 7});
        let a = shuffle_list(&args).unwrap();
        let b = shuffle_list(&args).unwrap();
        assert_eq!(a, b, "same items+seed -> same order");
        // It is a true permutation: same multiset of elements, no loss/dup.
        let body = a.split(" (seed").next().unwrap();
        let mut got: Vec<&str> = body.split(", ").collect();
        got.sort_unstable();
        assert_eq!(got, vec!["a", "b", "c", "d", "e"], "permutation preserves the set");
        // A different seed generally yields a different order.
        let c = shuffle_list(&json!({"items": ["a", "b", "c", "d", "e"], "seed": 8})).unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn shuffle_list_validates_input() {
        assert!(shuffle_list(&json!({"items": [], "seed": 1})).is_err(), "empty");
        assert!(shuffle_list(&json!({"items": ["a"]})).is_err(), "missing seed");
        assert!(shuffle_list(&json!({"items": [true], "seed": 1})).is_err(), "non-string");
    }

    // --- palindrome_check --------------------------------------------------

    #[test]
    fn palindrome_check_handles_phrases_and_case() {
        assert!(palindrome_check(&json!({"text": "racecar"})).unwrap().contains("is a palindrome"));
        // Ignores case, spaces, and punctuation.
        assert!(palindrome_check(&json!({"text": "A man, a plan, a canal: Panama"}))
            .unwrap()
            .contains("is a palindrome"));
        assert!(palindrome_check(&json!({"text": "hello"})).unwrap().contains("is not a palindrome"));
    }

    #[test]
    fn palindrome_check_rejects_empty_and_missing() {
        assert!(palindrome_check(&json!({})).is_err(), "missing text");
        assert!(palindrome_check(&json!({"text": "!!! ..."})).is_err(), "no letters/digits");
    }

    // --- anagram_check -----------------------------------------------------

    #[test]
    fn anagram_check_matches_real_anagrams() {
        assert!(anagram_check(&json!({"a": "listen", "b": "silent"})).unwrap().contains("are anagrams"));
        // Ignores case + spaces + punctuation.
        assert!(anagram_check(&json!({"a": "Dormitory", "b": "Dirty room"}))
            .unwrap()
            .contains("are anagrams"));
        assert!(anagram_check(&json!({"a": "hello", "b": "world"})).unwrap().contains("not anagrams"));
    }

    #[test]
    fn anagram_check_validates_input() {
        assert!(anagram_check(&json!({"a": "listen"})).is_err(), "missing b");
        assert!(anagram_check(&json!({"a": "...", "b": "abc"})).is_err(), "empty after cleaning");
    }

    // --- wordle_feedback ---------------------------------------------------

    #[test]
    fn wordle_all_green_when_guess_equals_answer() {
        let out = wordle_feedback(&json!({"guess": "crane", "answer": "crane"})).unwrap();
        assert!(out.contains("[GGGGG]"), "exact match is all green: {out}");
    }

    #[test]
    fn wordle_handles_duplicate_letters_correctly() {
        // Classic case: guess "speed" vs answer "abide". Only one 'e' is in the
        // answer, so exactly one of the guessed 'e's may be yellow, never both.
        let out = wordle_feedback(&json!({"guess": "speed", "answer": "abide"})).unwrap();
        let code = out.split('[').nth(1).unwrap().trim_end_matches(']');
        // s(-) p(-) e(Y, position 4 in answer) e(-, no copies left) d(Y? 'd' is in
        // 'abide') — verify the 'e' count and 'd' classification precisely.
        assert_eq!(code.matches('Y').count(), 2, "one e + the d are yellow: {out}");
        // The two 'e's at indices 2,3 cannot BOTH be yellow.
        let marks: Vec<char> = code.chars().collect();
        assert!(!(marks[2] == 'Y' && marks[3] == 'Y'), "duplicate e not double-counted");
        // Known full pattern for this pair.
        assert_eq!(code, "--Y-Y", "speed vs abide: {out}");
    }

    #[test]
    fn wordle_known_mixed_pattern() {
        // guess "weary" vs answer "wreck": w=G, e present(Y), a absent(-),
        // r present(Y), y absent(-).
        let out = wordle_feedback(&json!({"guess": "weary", "answer": "wreck"})).unwrap();
        let code = out.split('[').nth(1).unwrap().trim_end_matches(']');
        assert_eq!(code, "GY-Y-", "weary vs wreck: {out}");
    }

    #[test]
    fn wordle_rejects_bad_lengths() {
        assert!(wordle_feedback(&json!({"guess": "cat", "answer": "crane"})).is_err());
        assert!(wordle_feedback(&json!({"guess": "crane", "answer": "cran3"})).is_err(), "non-letter");
        assert!(wordle_feedback(&json!({"guess": "crane"})).is_err(), "missing answer");
    }

    // --- pig_latin ---------------------------------------------------------

    #[test]
    fn pig_latin_translates_words_by_the_rules() {
        // Consonant-initial: move the onset cluster, add "ay".
        assert_eq!(pig_latin_word("hello"), "ellohay");
        assert_eq!(pig_latin_word("smile"), "ilesmay");
        // Vowel-initial: add "way".
        assert_eq!(pig_latin_word("apple"), "appleway");
        // Capitalisation of the first letter is preserved.
        assert_eq!(pig_latin_word("Hello"), "Ellohay");
    }

    #[test]
    fn pig_latin_preserves_spacing_and_punctuation() {
        let out = pig_latin(&json!({"text": "hello world!"})).unwrap();
        assert_eq!(out, "ellohay orldway!");
        // Deterministic.
        let again = pig_latin(&json!({"text": "hello world!"})).unwrap();
        assert_eq!(out, again);
        assert!(pig_latin(&json!({})).is_err(), "missing text");
    }

    // --- leetspeak ---------------------------------------------------------

    #[test]
    fn leetspeak_encodes_and_decodes() {
        assert_eq!(leetspeak(&json!({"text": "leet"})).unwrap(), "1337");
        // a->4, e->3, s->5, t->7; unmapped letters (h, c, k, r, i) pass through.
        assert_eq!(leetspeak(&json!({"text": "elite hacker"})).unwrap(), "31i73 h4ck3r");
        // Decode is the inverse on the encoded leet digits.
        assert_eq!(leetspeak(&json!({"text": "1337", "decode": true})).unwrap(), "leet");
        assert!(leetspeak(&json!({})).is_err(), "missing text");
    }

    // --- ascii_banner ------------------------------------------------------

    #[test]
    fn ascii_banner_renders_five_rows_with_the_right_shape() {
        let out = ascii_banner(&json!({"text": "HI"})).unwrap();
        let rows: Vec<&str> = out.split('\n').collect();
        assert_eq!(rows.len(), 5, "exactly 5 rows");
        // Each row width = 2 glyphs * 5 + 1 gap = 11.
        assert!(rows.iter().all(|r| r.chars().count() == 11), "uniform width: {out}");
        // 'H' top row is its known pattern; banner is deterministic.
        assert!(out.starts_with("#   # #####"), "H then I top row: {out}");
        let again = ascii_banner(&json!({"text": "HI"})).unwrap();
        assert_eq!(out, again);
    }

    #[test]
    fn ascii_banner_validates_alphabet_and_bounds() {
        assert!(ascii_banner(&json!({"text": ""})).is_err(), "empty");
        assert!(ascii_banner(&json!({"text": "hi!"})).is_err(), "punctuation unsupported");
        assert!(ascii_banner(&json!({"text": "x".repeat(21)})).is_err(), "too long");
        assert!(ascii_banner(&json!({})).is_err(), "missing text");
        // Lowercase + space + digits are accepted.
        assert!(ascii_banner(&json!({"text": "a 1"})).is_ok());
    }

    // --- catalog -----------------------------------------------------------

    #[test]
    fn every_fun_skill_is_pure_and_well_formed() {
        let s = skills();
        assert_eq!(s.len(), 11, "the fun catalog ships 11 skills");
        for d in &s {
            assert_eq!(d.category, Category::Fun);
            assert!(!d.consequential && !d.source_gated, "{} is pure", d.name);
            assert!(!d.description.is_empty(), "{} has a description", d.name);
            assert!(super::super::is_snake_case(d.name), "{} is snake_case", d.name);
        }
        // No duplicate names within the category.
        let mut names: Vec<&str> = s.iter().map(|d| d.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "no duplicate fun skill names");
    }
}
