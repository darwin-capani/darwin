//! Category: KNOWLEDGE — reference lookups over BUNDLED, in-tree data (constants,
//! country/capital tables, the periodic table, the phonetic alphabet, HTTP status
//! codes, well-known unit prefixes). Every skill here is a PURE total function of
//! its args over data that is compiled into the binary — no network, no live feed,
//! no clock. A lookup whose honest answer would need a LIVE external source (an
//! online dictionary, current weather, a live FX rate) would be marked
//! `source_gated` and return a "needs a data source" notice — but everything in
//! THIS module is genuinely bundled, so nothing here is source-gated.
//!
//! Correctness over coverage: the bundled tables are small, hand-checked subsets
//! (not the whole world), and each skill says so honestly — a miss is a friendly
//! "not in the bundled table" error, NEVER a fabricated guess.

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{Category, SkillDef};

/// The knowledge catalog.
pub fn skills() -> Vec<SkillDef> {
    vec![
        SkillDef::new(
            "country_capital",
            Category::Knowledge,
            "Look up a country's capital (or which country a capital belongs to) from a bundled table. Use for capital-city questions about well-known countries.",
            &["capital of", "what's the capital", "capital city", "which country is the capital of"],
            country_capital,
        ),
        SkillDef::new(
            "periodic_element",
            Category::Knowledge,
            "Look up a chemical element by symbol, name, or atomic number from the bundled periodic table. Use for atomic number/symbol/name/atomic-weight questions.",
            &["element", "atomic number", "chemical symbol", "periodic table", "atomic weight"],
            periodic_element,
        ),
        SkillDef::new(
            "physical_constant",
            Category::Knowledge,
            "Look up a named physics/math constant (speed of light, Planck, Avogadro, pi, e, g, ...) with its value and unit from a bundled table. Use when the user needs the value of a constant.",
            &["speed of light", "planck constant", "avogadro", "value of pi", "gravitational constant", "constant"],
            physical_constant,
        ),
        SkillDef::new(
            "morse_code",
            Category::Knowledge,
            "Encode text to Morse code or decode Morse back to text (letters, digits, common punctuation). Use for Morse encode/decode requests.",
            &["morse code", "to morse", "decode morse", "encode morse", "dot dash"],
            morse_code,
        ),
        SkillDef::new(
            "nato_phonetic",
            Category::Knowledge,
            "Spell text out in the NATO phonetic alphabet (A->Alfa, B->Bravo, ...). Use to spell a word/code letter by letter over radio/phone.",
            &["nato phonetic", "phonetic alphabet", "spell it out", "alfa bravo charlie", "spell phonetically"],
            nato_phonetic,
        ),
        SkillDef::new(
            "http_status",
            Category::Knowledge,
            "Look up an HTTP status code's standard reason phrase and a short meaning (e.g. 404, 301, 503). Use to explain an HTTP status code.",
            &["http status", "status code", "what is a 404", "http 500", "response code"],
            http_status,
        ),
        SkillDef::new(
            "ascii_lookup",
            Category::Knowledge,
            "Convert between a character and its ASCII/Unicode code point (decimal + hex). Use to find a character's code or the character for a code point.",
            &["ascii code", "ascii value", "code point", "char code", "char for code"],
            ascii_lookup,
        ),
        SkillDef::new(
            "planet_fact",
            Category::Knowledge,
            "Look up bundled facts about a Solar-System planet (order, radius, day length, orbital period, moons). Use for basic planetary facts.",
            &["planet", "how big is mars", "planet facts", "how many moons", "orbital period"],
            planet_fact,
        ),
        SkillDef::new(
            "si_prefix",
            Category::Knowledge,
            "Look up an SI metric prefix by name or symbol and its power-of-ten factor (kilo=1e3, micro=1e-6, ...). Use to resolve a metric prefix.",
            &["si prefix", "metric prefix", "what is kilo", "micro prefix", "power of ten prefix"],
            si_prefix,
        ),
        SkillDef::new(
            "file_extension",
            Category::Knowledge,
            "Identify what a file extension is (e.g. .json, .png, .rs) — its kind and a short description — from a bundled table. Use to explain a file extension.",
            &["file extension", "what is a .json file", "file type", "extension meaning", "what opens a"],
            file_extension,
        ),
        SkillDef::new(
            "timezone_offset",
            Category::Knowledge,
            "Look up a fixed UTC offset for a well-known time-zone abbreviation (UTC, GMT, EST, PST, JST, ...). Use for the standard (non-DST) offset of a named zone. Bundled fixed offsets only — not a live clock.",
            &["utc offset", "timezone", "what is est", "gmt offset", "time zone offset"],
            timezone_offset,
        ),
    ]
}

// ---------------------------------------------------------------------------
// country_capital
// ---------------------------------------------------------------------------

/// (country, capital) for a bundled set of well-known countries. Hand-checked.
const CAPITALS: &[(&str, &str)] = &[
    ("france", "Paris"),
    ("germany", "Berlin"),
    ("italy", "Rome"),
    ("spain", "Madrid"),
    ("portugal", "Lisbon"),
    ("united kingdom", "London"),
    ("ireland", "Dublin"),
    ("netherlands", "Amsterdam"),
    ("belgium", "Brussels"),
    ("switzerland", "Bern"),
    ("austria", "Vienna"),
    ("sweden", "Stockholm"),
    ("norway", "Oslo"),
    ("denmark", "Copenhagen"),
    ("finland", "Helsinki"),
    ("poland", "Warsaw"),
    ("greece", "Athens"),
    ("russia", "Moscow"),
    ("united states", "Washington, D.C."),
    ("canada", "Ottawa"),
    ("mexico", "Mexico City"),
    ("brazil", "Brasília"),
    ("argentina", "Buenos Aires"),
    ("chile", "Santiago"),
    ("china", "Beijing"),
    ("japan", "Tokyo"),
    ("south korea", "Seoul"),
    ("india", "New Delhi"),
    ("australia", "Canberra"),
    ("new zealand", "Wellington"),
    ("egypt", "Cairo"),
    ("south africa", "Pretoria"),
    ("nigeria", "Abuja"),
    ("kenya", "Nairobi"),
    ("turkey", "Ankara"),
    ("saudi arabia", "Riyadh"),
    ("israel", "Jerusalem"),
    ("united arab emirates", "Abu Dhabi"),
];

/// `country_capital {country?}` or `{capital?}` -> resolves either direction over
/// the bundled table. Case/whitespace-insensitive. A miss is a friendly error, not
/// a fabricated guess.
fn country_capital(args: &Value) -> Result<String> {
    if let Some(country) = args.get("country").and_then(Value::as_str) {
        let key = country.trim().to_ascii_lowercase();
        return CAPITALS
            .iter()
            .find(|(c, _)| *c == key)
            .map(|(c, cap)| format!("The capital of {} is {}.", title_case(c), cap))
            .ok_or_else(|| anyhow!("'{}' is not in the bundled country table", country.trim()));
    }
    if let Some(capital) = args.get("capital").and_then(Value::as_str) {
        let key = capital.trim().to_ascii_lowercase();
        return CAPITALS
            .iter()
            .find(|(_, cap)| cap.to_ascii_lowercase() == key)
            .map(|(c, cap)| format!("{} is the capital of {}.", cap, title_case(c)))
            .ok_or_else(|| anyhow!("'{}' is not a capital in the bundled table", capital.trim()));
    }
    Err(anyhow!("country_capital needs a 'country' or 'capital' string argument"))
}

/// Title-case a lowercase ASCII multi-word key for display ("united kingdom" ->
/// "United Kingdom"). Pure; only used for our own bundled keys.
fn title_case(s: &str) -> String {
    s.split(' ')
        .map(|w| {
            let mut ch = w.chars();
            match ch.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + ch.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// periodic_element
// ---------------------------------------------------------------------------

/// (atomic number, symbol, name, standard atomic weight). A bundled subset of the
/// periodic table: the first 30 elements plus a few well-known heavier ones.
/// Weights are the CIAAW standard atomic weights (4 sig figs).
const ELEMENTS: &[(u8, &str, &str, f64)] = &[
    (1, "H", "Hydrogen", 1.008),
    (2, "He", "Helium", 4.003),
    (3, "Li", "Lithium", 6.94),
    (4, "Be", "Beryllium", 9.012),
    (5, "B", "Boron", 10.81),
    (6, "C", "Carbon", 12.011),
    (7, "N", "Nitrogen", 14.007),
    (8, "O", "Oxygen", 15.999),
    (9, "F", "Fluorine", 18.998),
    (10, "Ne", "Neon", 20.180),
    (11, "Na", "Sodium", 22.990),
    (12, "Mg", "Magnesium", 24.305),
    (13, "Al", "Aluminium", 26.982),
    (14, "Si", "Silicon", 28.085),
    (15, "P", "Phosphorus", 30.974),
    (16, "S", "Sulfur", 32.06),
    (17, "Cl", "Chlorine", 35.45),
    (18, "Ar", "Argon", 39.95),
    (19, "K", "Potassium", 39.098),
    (20, "Ca", "Calcium", 40.078),
    (21, "Sc", "Scandium", 44.956),
    (22, "Ti", "Titanium", 47.867),
    (23, "V", "Vanadium", 50.942),
    (24, "Cr", "Chromium", 51.996),
    (25, "Mn", "Manganese", 54.938),
    (26, "Fe", "Iron", 55.845),
    (27, "Co", "Cobalt", 58.933),
    (28, "Ni", "Nickel", 58.693),
    (29, "Cu", "Copper", 63.546),
    (30, "Zn", "Zinc", 65.38),
    (47, "Ag", "Silver", 107.868),
    (79, "Au", "Gold", 196.967),
    (80, "Hg", "Mercury", 200.592),
    (82, "Pb", "Lead", 207.2),
    (92, "U", "Uranium", 238.029),
];

/// `periodic_element {symbol?|name?|number?}` -> a one-line fact. Accepts a symbol
/// ("Fe"), an element name ("iron"), or an atomic number (26). Case-insensitive.
fn periodic_element(args: &Value) -> Result<String> {
    let found = if let Some(sym) = args.get("symbol").and_then(Value::as_str) {
        let k = sym.trim().to_ascii_lowercase();
        ELEMENTS.iter().find(|(_, s, _, _)| s.to_ascii_lowercase() == k)
    } else if let Some(name) = args.get("name").and_then(Value::as_str) {
        let k = name.trim().to_ascii_lowercase();
        ELEMENTS.iter().find(|(_, _, n, _)| n.to_ascii_lowercase() == k)
    } else if let Some(num) = args.get("number").and_then(Value::as_u64) {
        ELEMENTS.iter().find(|(z, _, _, _)| *z as u64 == num)
    } else {
        return Err(anyhow!(
            "periodic_element needs a 'symbol', 'name', or 'number' argument"
        ));
    };
    found
        .map(|(z, s, n, w)| format!("{n} ({s}): atomic number {z}, standard atomic weight {w}."))
        .ok_or_else(|| anyhow!("that element is not in the bundled periodic table"))
}

// ---------------------------------------------------------------------------
// physical_constant
// ---------------------------------------------------------------------------

/// (key, display name, value, unit). Bundled physics/math constants. SI-defined
/// values are exact (the 2019 redefinition); measured ones are CODATA. Math
/// constants carry an empty unit.
const CONSTANTS: &[(&str, &str, &str, &str)] = &[
    ("speed_of_light", "speed of light in vacuum (c)", "299792458", "m/s"),
    ("planck", "Planck constant (h)", "6.62607015e-34", "J·s"),
    ("reduced_planck", "reduced Planck constant (ħ)", "1.054571817e-34", "J·s"),
    ("gravitational", "Newtonian gravitational constant (G)", "6.67430e-11", "m³·kg⁻¹·s⁻²"),
    ("elementary_charge", "elementary charge (e)", "1.602176634e-19", "C"),
    ("avogadro", "Avogadro constant (N_A)", "6.02214076e23", "mol⁻¹"),
    ("boltzmann", "Boltzmann constant (k_B)", "1.380649e-23", "J/K"),
    ("gas_constant", "molar gas constant (R)", "8.314462618", "J·mol⁻¹·K⁻¹"),
    ("electron_mass", "electron mass (m_e)", "9.1093837015e-31", "kg"),
    ("proton_mass", "proton mass (m_p)", "1.67262192369e-27", "kg"),
    ("standard_gravity", "standard acceleration of gravity (g₀)", "9.80665", "m/s²"),
    ("pi", "pi (π)", "3.14159265358979323846", ""),
    ("e", "Euler's number (e)", "2.71828182845904523536", ""),
    ("golden_ratio", "golden ratio (φ)", "1.61803398874989484820", ""),
    ("sqrt2", "square root of 2 (√2)", "1.41421356237309504880", ""),
];

/// `physical_constant {name}` -> "<display name> = <value> <unit>". `name` matches
/// either the snake_case key ("speed_of_light") or a few common aliases. A miss is
/// a friendly error listing nothing fabricated.
fn physical_constant(args: &Value) -> Result<String> {
    let raw = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("physical_constant needs a 'name' string argument"))?;
    let key = normalize_constant_key(raw);
    CONSTANTS
        .iter()
        .find(|(k, _, _, _)| *k == key)
        .map(|(_, disp, val, unit)| {
            if unit.is_empty() {
                format!("{disp} = {val}")
            } else {
                format!("{disp} = {val} {unit}")
            }
        })
        .ok_or_else(|| anyhow!("'{}' is not in the bundled constants table", raw.trim()))
}

/// Map a free-form constant name to a canonical key. Lowercases, collapses spaces
/// to underscores, and folds a handful of common aliases. Pure.
fn normalize_constant_key(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    match lower.as_str() {
        "c" | "light speed" | "lightspeed" => return "speed_of_light".into(),
        "h" => return "planck".into(),
        "hbar" | "h-bar" | "h_bar" => return "reduced_planck".into(),
        "g" if lower == "g" => return "gravitational".into(),
        "na" | "n_a" => return "avogadro".into(),
        "kb" | "k_b" => return "boltzmann".into(),
        "r" => return "gas_constant".into(),
        "phi" => return "golden_ratio".into(),
        "g0" | "g_0" | "gravity" => return "standard_gravity".into(),
        _ => {}
    }
    lower.replace([' ', '-'], "_")
}

// ---------------------------------------------------------------------------
// morse_code
// ---------------------------------------------------------------------------

/// (char, morse) for letters, digits, and common punctuation (ITU). Uppercase
/// letters only; the encoder upper-cases input.
const MORSE: &[(char, &str)] = &[
    ('A', ".-"), ('B', "-..."), ('C', "-.-."), ('D', "-.."), ('E', "."),
    ('F', "..-."), ('G', "--."), ('H', "...."), ('I', ".."), ('J', ".---"),
    ('K', "-.-"), ('L', ".-.."), ('M', "--"), ('N', "-."), ('O', "---"),
    ('P', ".--."), ('Q', "--.-"), ('R', ".-."), ('S', "..."), ('T', "-"),
    ('U', "..-"), ('V', "...-"), ('W', ".--"), ('X', "-..-"), ('Y', "-.--"),
    ('Z', "--.."),
    ('0', "-----"), ('1', ".----"), ('2', "..---"), ('3', "...--"),
    ('4', "....-"), ('5', "....."), ('6', "-...."), ('7', "--..."),
    ('8', "---.."), ('9', "----."),
    ('.', ".-.-.-"), (',', "--..--"), ('?', "..--.."), ('\'', ".----."),
    ('!', "-.-.--"), ('/', "-..-."), ('(', "-.--."), (')', "-.--.-"),
    ('&', ".-..."), (':', "---..."), (';', "-.-.-."), ('=', "-...-"),
    ('+', ".-.-."), ('-', "-....-"), ('"', ".-..-."), ('@', ".--.-."),
];

/// `morse_code {direction, text}` -> encode/decode. `direction` is "encode"
/// (text -> Morse: letters separated by spaces, words by " / ") or "decode"
/// (Morse -> text: codes separated by spaces, words by "/" or "   "). Unknown
/// characters/codes are a friendly error — never silently dropped.
fn morse_code(args: &Value) -> Result<String> {
    let direction = args
        .get("direction")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("morse_code needs a 'direction' of \"encode\" or \"decode\""))?;
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("morse_code needs a 'text' string argument"))?;
    match direction.trim().to_ascii_lowercase().as_str() {
        "encode" => morse_encode(text),
        "decode" => morse_decode(text),
        other => Err(anyhow!("morse_code 'direction' must be \"encode\" or \"decode\", got '{other}'")),
    }
}

/// Encode plain text to Morse. Words (whitespace-separated) join with " / ";
/// letters within a word join with a single space. An unsupported character is a
/// friendly error.
fn morse_encode(text: &str) -> Result<String> {
    let mut words = Vec::new();
    for word in text.split_whitespace() {
        let mut codes = Vec::new();
        for ch in word.chars() {
            let up = ch.to_ascii_uppercase();
            let code = MORSE
                .iter()
                .find(|(c, _)| *c == up)
                .map(|(_, m)| *m)
                .ok_or_else(|| anyhow!("character '{ch}' has no Morse encoding"))?;
            codes.push(code);
        }
        words.push(codes.join(" "));
    }
    if words.is_empty() {
        return Err(anyhow!("morse_code encode needs non-empty text"));
    }
    Ok(words.join(" / "))
}

/// Decode Morse to plain text. Words separate on "/" (optionally surrounded by
/// spaces) or a run of 3+ spaces; letters within a word separate on single spaces.
/// An unknown code is a friendly error.
fn morse_decode(text: &str) -> Result<String> {
    let mut out = String::new();
    let words: Vec<&str> = text.split('/').collect();
    for (wi, word) in words.iter().enumerate() {
        if wi > 0 {
            out.push(' ');
        }
        for code in word.split_whitespace() {
            let ch = MORSE
                .iter()
                .find(|(_, m)| *m == code)
                .map(|(c, _)| *c)
                .ok_or_else(|| anyhow!("'{code}' is not a known Morse code"))?;
            out.push(ch);
        }
    }
    if out.trim().is_empty() {
        return Err(anyhow!("morse_code decode needs non-empty Morse input"));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// nato_phonetic
// ---------------------------------------------------------------------------

/// NATO phonetic words for A–Z, plus the ICAO digit words. Pure lookup.
const NATO: &[(char, &str)] = &[
    ('A', "Alfa"), ('B', "Bravo"), ('C', "Charlie"), ('D', "Delta"),
    ('E', "Echo"), ('F', "Foxtrot"), ('G', "Golf"), ('H', "Hotel"),
    ('I', "India"), ('J', "Juliett"), ('K', "Kilo"), ('L', "Lima"),
    ('M', "Mike"), ('N', "November"), ('O', "Oscar"), ('P', "Papa"),
    ('Q', "Quebec"), ('R', "Romeo"), ('S', "Sierra"), ('T', "Tango"),
    ('U', "Uniform"), ('V', "Victor"), ('W', "Whiskey"), ('X', "Xray"),
    ('Y', "Yankee"), ('Z', "Zulu"),
    ('0', "Zero"), ('1', "One"), ('2', "Two"), ('3', "Three"),
    ('4', "Four"), ('5', "Five"), ('6', "Six"), ('7', "Seven"),
    ('8', "Eight"), ('9', "Nine"),
];

/// `nato_phonetic {text}` -> the input spelled in NATO words, space-joined. Spaces
/// in the input become "(space)" markers; an unsupported character is a friendly
/// error.
fn nato_phonetic(args: &Value) -> Result<String> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("nato_phonetic needs a 'text' string argument"))?;
    if text.is_empty() {
        return Err(anyhow!("nato_phonetic needs non-empty text"));
    }
    let mut parts = Vec::new();
    for ch in text.chars() {
        if ch == ' ' {
            parts.push("(space)".to_string());
            continue;
        }
        let up = ch.to_ascii_uppercase();
        let word = NATO
            .iter()
            .find(|(c, _)| *c == up)
            .map(|(_, w)| *w)
            .ok_or_else(|| anyhow!("character '{ch}' has no NATO phonetic word"))?;
        parts.push(word.to_string());
    }
    Ok(parts.join(" "))
}

// ---------------------------------------------------------------------------
// http_status
// ---------------------------------------------------------------------------

/// (code, reason phrase, short meaning) for the common HTTP status codes (RFC
/// 9110 + a few widely used extras). Bundled subset, hand-checked.
const HTTP_STATUS: &[(u16, &str, &str)] = &[
    (100, "Continue", "the client should continue the request"),
    (101, "Switching Protocols", "the server is switching protocols as requested"),
    (200, "OK", "the request succeeded"),
    (201, "Created", "the request succeeded and a new resource was created"),
    (202, "Accepted", "the request was accepted for processing, not yet complete"),
    (204, "No Content", "success with no response body"),
    (206, "Partial Content", "the server is delivering part of the resource (range request)"),
    (301, "Moved Permanently", "the resource has a new permanent URL"),
    (302, "Found", "the resource is temporarily at a different URL"),
    (304, "Not Modified", "the cached copy is still valid"),
    (307, "Temporary Redirect", "temporary redirect, method preserved"),
    (308, "Permanent Redirect", "permanent redirect, method preserved"),
    (400, "Bad Request", "the server could not understand the request"),
    (401, "Unauthorized", "authentication is required or failed"),
    (403, "Forbidden", "authenticated but not allowed to access this"),
    (404, "Not Found", "the resource does not exist"),
    (405, "Method Not Allowed", "the HTTP method is not supported for this resource"),
    (408, "Request Timeout", "the client took too long to send the request"),
    (409, "Conflict", "the request conflicts with the current state"),
    (410, "Gone", "the resource is permanently gone"),
    (418, "I'm a teapot", "an April Fools' joke code (RFC 2324)"),
    (422, "Unprocessable Content", "the request was well-formed but semantically invalid"),
    (429, "Too Many Requests", "the client has sent too many requests (rate limited)"),
    (500, "Internal Server Error", "a generic server-side failure"),
    (501, "Not Implemented", "the server does not support this functionality"),
    (502, "Bad Gateway", "an upstream server returned an invalid response"),
    (503, "Service Unavailable", "the server is temporarily overloaded or down"),
    (504, "Gateway Timeout", "an upstream server did not respond in time"),
];

/// `http_status {code}` -> "<code> <reason> — <meaning>". A code outside the
/// bundled table is a friendly error, not a guess.
fn http_status(args: &Value) -> Result<String> {
    let code = args
        .get("code")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("http_status needs an integer 'code' argument (e.g. 404)"))?;
    if !(100..=599).contains(&code) {
        return Err(anyhow!("'{code}' is not a valid HTTP status code (100..=599)"));
    }
    HTTP_STATUS
        .iter()
        .find(|(c, _, _)| *c as u64 == code)
        .map(|(c, reason, meaning)| format!("{c} {reason} — {meaning}."))
        .ok_or_else(|| anyhow!("HTTP {code} is not in the bundled status-code table"))
}

// ---------------------------------------------------------------------------
// ascii_lookup
// ---------------------------------------------------------------------------

/// `ascii_lookup {char?|code?}` -> converts between a character and its code point.
/// Given a single-character `char`, reports its decimal + hex code point. Given an
/// integer `code` (0..=0x10FFFF), reports the character (with a name for the
/// common non-printables). "ASCII" in the name reflects the common case; full
/// Unicode scalar values are accepted.
fn ascii_lookup(args: &Value) -> Result<String> {
    if let Some(s) = args.get("char").and_then(Value::as_str) {
        let mut chars = s.chars();
        let ch = chars
            .next()
            .ok_or_else(|| anyhow!("ascii_lookup 'char' must be a single character"))?;
        if chars.next().is_some() {
            return Err(anyhow!("ascii_lookup 'char' must be exactly one character"));
        }
        let cp = ch as u32;
        return Ok(format!(
            "'{}' -> code point {cp} (0x{cp:04X})",
            display_char(ch)
        ));
    }
    if let Some(code) = args.get("code").and_then(Value::as_u64) {
        if code > 0x10FFFF {
            return Err(anyhow!("'{code}' is past the maximum Unicode code point (0x10FFFF)"));
        }
        let cp = code as u32;
        // Surrogate range is not a valid scalar value.
        let ch = char::from_u32(cp)
            .ok_or_else(|| anyhow!("code point {cp} (0x{cp:04X}) is not a valid character (surrogate)"))?;
        return Ok(format!(
            "code point {cp} (0x{cp:04X}) -> '{}'",
            display_char(ch)
        ));
    }
    Err(anyhow!("ascii_lookup needs a single-character 'char' or an integer 'code'"))
}

/// Render a char for display, naming the common control codes so the output is
/// readable rather than emitting a raw control byte. Pure.
fn display_char(ch: char) -> String {
    match ch {
        ' ' => "(space)".to_string(),
        '\0' => "(NUL)".to_string(),
        '\t' => "(TAB)".to_string(),
        '\n' => "(LF)".to_string(),
        '\r' => "(CR)".to_string(),
        c if (c as u32) < 0x20 || c as u32 == 0x7F => format!("(control 0x{:02X})", c as u32),
        c => c.to_string(),
    }
}

// ---------------------------------------------------------------------------
// planet_fact
// ---------------------------------------------------------------------------

/// (name, order from Sun, mean radius km, sidereal day hours, orbital period
/// Earth-days, moon count). Bundled, hand-checked Solar-System data.
const PLANETS: &[(&str, u8, f64, f64, f64, u32)] = &[
    ("mercury", 1, 2439.7, 1407.6, 88.0, 0),
    ("venus", 2, 6051.8, 5832.5, 224.7, 0),
    ("earth", 3, 6371.0, 23.9, 365.2, 1),
    ("mars", 4, 3389.5, 24.6, 687.0, 2),
    ("jupiter", 5, 69911.0, 9.9, 4331.0, 95),
    ("saturn", 6, 58232.0, 10.7, 10747.0, 146),
    ("uranus", 7, 25362.0, 17.2, 30589.0, 28),
    ("neptune", 8, 24622.0, 16.1, 59800.0, 16),
];

/// `planet_fact {planet}` -> a one-line fact sheet. Case-insensitive. Pluto and
/// non-planets are an honest "not in the bundled table" error.
fn planet_fact(args: &Value) -> Result<String> {
    let name = args
        .get("planet")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("planet_fact needs a 'planet' string argument"))?;
    let key = name.trim().to_ascii_lowercase();
    PLANETS
        .iter()
        .find(|(n, _, _, _, _, _)| *n == key)
        .map(|(n, order, radius, day, period, moons)| {
            format!(
                "{} — planet #{order} from the Sun: mean radius {radius} km, day {day} h, orbital period {period} Earth-days, {moons} known moons.",
                title_case(n)
            )
        })
        .ok_or_else(|| anyhow!("'{}' is not one of the eight planets in the bundled table", name.trim()))
}

// ---------------------------------------------------------------------------
// si_prefix
// ---------------------------------------------------------------------------

/// (name, symbol, base-10 exponent). The full set of SI decimal prefixes,
/// quecto(-30) through quetta(30).
const SI_PREFIXES: &[(&str, &str, i32)] = &[
    ("quetta", "Q", 30),
    ("ronna", "R", 27),
    ("yotta", "Y", 24),
    ("zetta", "Z", 21),
    ("exa", "E", 18),
    ("peta", "P", 15),
    ("tera", "T", 12),
    ("giga", "G", 9),
    ("mega", "M", 6),
    ("kilo", "k", 3),
    ("hecto", "h", 2),
    ("deca", "da", 1),
    ("deci", "d", -1),
    ("centi", "c", -2),
    ("milli", "m", -3),
    ("micro", "µ", -6),
    ("nano", "n", -9),
    ("pico", "p", -12),
    ("femto", "f", -15),
    ("atto", "a", -18),
    ("zepto", "z", -21),
    ("yocto", "y", -24),
    ("ronto", "r", -27),
    ("quecto", "q", -30),
];

/// `si_prefix {prefix}` -> name, symbol, and factor (10^exp). Matches a prefix
/// name ("kilo") or symbol ("k", "µ", or ASCII "u" for micro). A miss is a
/// friendly error.
fn si_prefix(args: &Value) -> Result<String> {
    let raw = args
        .get("prefix")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("si_prefix needs a 'prefix' string argument (name or symbol)"))?;
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    // ASCII "u" is the conventional stand-in for the micro sign µ.
    let found = SI_PREFIXES.iter().find(|(name, sym, _)| {
        name.eq_ignore_ascii_case(trimmed)
            || *sym == trimmed
            || (lower == "u" && *name == "micro")
    });
    found
        .map(|(name, sym, exp)| {
            format!("{name} ({sym}) = 10^{exp} = {}", power_of_ten(*exp))
        })
        .ok_or_else(|| anyhow!("'{trimmed}' is not a known SI prefix"))
}

/// Render 10^exp as a readable factor string. For exponents in a small range it is
/// written out (e.g. "1000", "0.001"); outside that it stays in 1e<exp> form to
/// avoid an absurdly long literal. Pure.
fn power_of_ten(exp: i32) -> String {
    if (-4..=6).contains(&exp) {
        if exp >= 0 {
            // 1 followed by `exp` zeros.
            let mut s = String::from("1");
            s.extend(std::iter::repeat_n('0', exp as usize));
            s
        } else {
            // 0.000…1 with |exp| decimal places.
            let zeros = (-exp - 1) as usize;
            let mut s = String::from("0.");
            s.extend(std::iter::repeat_n('0', zeros));
            s.push('1');
            s
        }
    } else {
        // Outside the spell-out range, use scientific notation. `exp` already
        // carries its sign (e.g. `1e10`, `1e-8`), so both directions render the same.
        format!("1e{exp}")
    }
}

// ---------------------------------------------------------------------------
// file_extension
// ---------------------------------------------------------------------------

/// (extension without dot, kind, description). Bundled, hand-checked table of
/// common file extensions.
const EXTENSIONS: &[(&str, &str, &str)] = &[
    ("txt", "text", "plain text"),
    ("md", "text", "Markdown formatted text"),
    ("csv", "data", "comma-separated values"),
    ("tsv", "data", "tab-separated values"),
    ("json", "data", "JSON data"),
    ("yaml", "data", "YAML data"),
    ("yml", "data", "YAML data"),
    ("toml", "config", "TOML configuration"),
    ("xml", "data", "XML markup"),
    ("html", "web", "HTML web page"),
    ("css", "web", "CSS stylesheet"),
    ("js", "code", "JavaScript source"),
    ("ts", "code", "TypeScript source"),
    ("py", "code", "Python source"),
    ("rs", "code", "Rust source"),
    ("go", "code", "Go source"),
    ("c", "code", "C source"),
    ("cpp", "code", "C++ source"),
    ("java", "code", "Java source"),
    ("sh", "code", "shell script"),
    ("png", "image", "PNG raster image"),
    ("jpg", "image", "JPEG raster image"),
    ("jpeg", "image", "JPEG raster image"),
    ("gif", "image", "GIF image"),
    ("svg", "image", "SVG vector image"),
    ("webp", "image", "WebP image"),
    ("mp3", "audio", "MP3 audio"),
    ("wav", "audio", "WAV audio"),
    ("flac", "audio", "FLAC lossless audio"),
    ("mp4", "video", "MP4 video"),
    ("mov", "video", "QuickTime video"),
    ("mkv", "video", "Matroska video"),
    ("pdf", "document", "PDF document"),
    ("docx", "document", "Microsoft Word document"),
    ("xlsx", "document", "Microsoft Excel spreadsheet"),
    ("pptx", "document", "Microsoft PowerPoint presentation"),
    ("zip", "archive", "ZIP archive"),
    ("tar", "archive", "tar archive"),
    ("gz", "archive", "gzip-compressed file"),
    ("7z", "archive", "7-Zip archive"),
];

/// `file_extension {ext}` -> "<.ext>: <kind> — <description>". A leading dot is
/// optional; matching is case-insensitive. A miss is a friendly error.
fn file_extension(args: &Value) -> Result<String> {
    let raw = args
        .get("ext")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("file_extension needs an 'ext' string argument (e.g. \".json\")"))?;
    let key = raw.trim().trim_start_matches('.').to_ascii_lowercase();
    if key.is_empty() {
        return Err(anyhow!("file_extension 'ext' is empty"));
    }
    EXTENSIONS
        .iter()
        .find(|(e, _, _)| *e == key)
        .map(|(e, kind, desc)| format!(".{e}: {kind} — {desc}."))
        .ok_or_else(|| anyhow!("'.{key}' is not in the bundled file-extension table"))
}

// ---------------------------------------------------------------------------
// timezone_offset
// ---------------------------------------------------------------------------

/// (abbreviation, fixed UTC offset in minutes, full name). STANDARD (non-DST)
/// offsets only — this is a static reference table, NOT a live clock and NOT
/// DST-aware. Hand-checked subset of widely used zone abbreviations.
const TIMEZONES: &[(&str, i32, &str)] = &[
    ("UTC", 0, "Coordinated Universal Time"),
    ("GMT", 0, "Greenwich Mean Time"),
    ("WET", 0, "Western European Time"),
    ("CET", 60, "Central European Time"),
    ("EET", 120, "Eastern European Time"),
    ("MSK", 180, "Moscow Standard Time"),
    ("IST", 330, "India Standard Time"),
    ("CST_CHINA", 480, "China Standard Time"),
    ("JST", 540, "Japan Standard Time"),
    ("KST", 540, "Korea Standard Time"),
    ("AEST", 600, "Australian Eastern Standard Time"),
    ("NZST", 720, "New Zealand Standard Time"),
    ("EST", -300, "Eastern Standard Time (North America)"),
    ("CST", -360, "Central Standard Time (North America)"),
    ("MST", -420, "Mountain Standard Time (North America)"),
    ("PST", -480, "Pacific Standard Time (North America)"),
    ("AKST", -540, "Alaska Standard Time"),
    ("HST", -600, "Hawaii Standard Time"),
];

/// `timezone_offset {abbr}` -> "<ABBR> (<name>) = UTC±HH:MM". Standard offset only,
/// from a bundled table. Case-insensitive on the abbreviation. A miss is a
/// friendly error — this never invents an offset.
fn timezone_offset(args: &Value) -> Result<String> {
    let raw = args
        .get("abbr")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("timezone_offset needs an 'abbr' string argument (e.g. \"PST\")"))?;
    let key = raw.trim().to_ascii_uppercase();
    TIMEZONES
        .iter()
        .find(|(a, _, _)| *a == key)
        .map(|(a, mins, name)| {
            let label = a.strip_suffix("_CHINA").unwrap_or(a);
            format!("{label} ({name}) = {}", format_offset(*mins))
        })
        .ok_or_else(|| anyhow!("'{}' is not in the bundled time-zone table", raw.trim()))
}

/// Format an offset-in-minutes as "UTC", "UTC+05:30", or "UTC-08:00". Pure.
fn format_offset(mins: i32) -> String {
    if mins == 0 {
        return "UTC".to_string();
    }
    let sign = if mins > 0 { '+' } else { '-' };
    let abs = mins.unsigned_abs();
    format!("UTC{sign}{:02}:{:02}", abs / 60, abs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_catalog_is_eleven_pure_knowledge_skills() {
        let s = skills();
        assert_eq!(s.len(), 11, "knowledge ships 11 skills");
        assert!(
            s.iter().all(|d| d.category == Category::Knowledge),
            "all categorized Knowledge"
        );
        assert!(
            s.iter().all(|d| !d.consequential && !d.source_gated),
            "every knowledge skill is pure + bundled (not consequential, not source-gated)"
        );
        // Names are exactly what we expect, no dups.
        let names: Vec<&str> = s.iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            vec![
                "country_capital",
                "periodic_element",
                "physical_constant",
                "morse_code",
                "nato_phonetic",
                "http_status",
                "ascii_lookup",
                "planet_fact",
                "si_prefix",
                "file_extension",
                "timezone_offset",
            ]
        );
    }

    #[test]
    fn country_capital_resolves_both_directions() {
        assert_eq!(
            country_capital(&json!({"country": "France"})).unwrap(),
            "The capital of France is Paris."
        );
        // Case + whitespace insensitive, multi-word country.
        assert_eq!(
            country_capital(&json!({"country": "  united kingdom "})).unwrap(),
            "The capital of United Kingdom is London."
        );
        // Reverse lookup.
        assert_eq!(
            country_capital(&json!({"capital": "Tokyo"})).unwrap(),
            "Tokyo is the capital of Japan."
        );
        // Unknown country / missing arg are friendly errors, not guesses.
        assert!(country_capital(&json!({"country": "Atlantis"})).is_err());
        assert!(country_capital(&json!({})).is_err());
    }

    #[test]
    fn periodic_element_by_symbol_name_and_number() {
        assert_eq!(
            periodic_element(&json!({"symbol": "Fe"})).unwrap(),
            "Iron (Fe): atomic number 26, standard atomic weight 55.845."
        );
        // Case-insensitive name.
        assert_eq!(
            periodic_element(&json!({"name": "oxygen"})).unwrap(),
            "Oxygen (O): atomic number 8, standard atomic weight 15.999."
        );
        // By atomic number.
        assert_eq!(
            periodic_element(&json!({"number": 79})).unwrap(),
            "Gold (Au): atomic number 79, standard atomic weight 196.967."
        );
        // Hydrogen is #1.
        assert!(periodic_element(&json!({"symbol": "H"})).unwrap().contains("atomic number 1,"));
        // Misses + missing args are errors.
        assert!(periodic_element(&json!({"symbol": "Xx"})).is_err());
        assert!(periodic_element(&json!({"number": 7777})).is_err());
        assert!(periodic_element(&json!({})).is_err());
    }

    #[test]
    fn physical_constant_known_values_and_aliases() {
        assert_eq!(
            physical_constant(&json!({"name": "speed_of_light"})).unwrap(),
            "speed of light in vacuum (c) = 299792458 m/s"
        );
        // Alias resolution.
        assert_eq!(
            physical_constant(&json!({"name": "c"})).unwrap(),
            "speed of light in vacuum (c) = 299792458 m/s"
        );
        // Math constant has no unit.
        assert_eq!(
            physical_constant(&json!({"name": "pi"})).unwrap(),
            "pi (π) = 3.14159265358979323846"
        );
        // Space-separated name normalizes to the key.
        assert!(physical_constant(&json!({"name": "gas constant"}))
            .unwrap()
            .contains("8.314462618"));
        assert!(physical_constant(&json!({"name": "not a constant"})).is_err());
        assert!(physical_constant(&json!({})).is_err());
    }

    #[test]
    fn morse_code_round_trips() {
        // Encode known vector.
        assert_eq!(
            morse_code(&json!({"direction": "encode", "text": "SOS"})).unwrap(),
            "... --- ..."
        );
        // Word separation with " / ".
        assert_eq!(
            morse_code(&json!({"direction": "encode", "text": "HI BOB"})).unwrap(),
            ".... .. / -... --- -..."
        );
        // Decode is the inverse.
        assert_eq!(
            morse_code(&json!({"direction": "decode", "text": "... --- ..."})).unwrap(),
            "SOS"
        );
        assert_eq!(
            morse_code(&json!({"direction": "decode", "text": ".... .. / -... --- -..."})).unwrap(),
            "HI BOB"
        );
        // Full round-trip of a mixed string.
        let original = "HELLO 123";
        let encoded = morse_code(&json!({"direction": "encode", "text": original})).unwrap();
        let decoded = morse_code(&json!({"direction": "decode", "text": encoded})).unwrap();
        assert_eq!(decoded, original);
        // Bad direction, unknown char/code, missing args.
        assert!(morse_code(&json!({"direction": "encode", "text": "café"})).is_err());
        assert!(morse_code(&json!({"direction": "decode", "text": "...---...-."})).is_err());
        assert!(morse_code(&json!({"direction": "sideways", "text": "x"})).is_err());
        assert!(morse_code(&json!({"text": "x"})).is_err());
    }

    #[test]
    fn nato_phonetic_spells_correctly() {
        assert_eq!(
            nato_phonetic(&json!({"text": "ABC"})).unwrap(),
            "Alfa Bravo Charlie"
        );
        // Digits + lowercase + space.
        assert_eq!(
            nato_phonetic(&json!({"text": "a1 b"})).unwrap(),
            "Alfa One (space) Bravo"
        );
        assert!(nato_phonetic(&json!({"text": "ç"})).is_err());
        assert!(nato_phonetic(&json!({"text": ""})).is_err());
        assert!(nato_phonetic(&json!({})).is_err());
    }

    #[test]
    fn http_status_known_codes() {
        assert_eq!(
            http_status(&json!({"code": 404})).unwrap(),
            "404 Not Found — the resource does not exist."
        );
        assert!(http_status(&json!({"code": 301})).unwrap().starts_with("301 Moved Permanently"));
        assert!(http_status(&json!({"code": 503})).unwrap().starts_with("503 Service Unavailable"));
        // In-range but not in the bundled table -> honest miss.
        assert!(http_status(&json!({"code": 499})).is_err());
        // Out of the valid range -> friendly error.
        assert!(http_status(&json!({"code": 999})).is_err());
        assert!(http_status(&json!({})).is_err());
    }

    #[test]
    fn ascii_lookup_both_directions() {
        assert_eq!(
            ascii_lookup(&json!({"char": "A"})).unwrap(),
            "'A' -> code point 65 (0x0041)"
        );
        assert_eq!(
            ascii_lookup(&json!({"code": 65})).unwrap(),
            "code point 65 (0x0041) -> 'A'"
        );
        // Space is named, not emitted raw.
        assert_eq!(
            ascii_lookup(&json!({"char": " "})).unwrap(),
            "'(space)' -> code point 32 (0x0020)"
        );
        // Non-ASCII Unicode scalar is fine.
        assert!(ascii_lookup(&json!({"code": 0x1F600u64}))
            .unwrap()
            .contains("0x1F600"));
        // Multi-char, surrogate, and out-of-range are errors; missing args too.
        assert!(ascii_lookup(&json!({"char": "AB"})).is_err());
        assert!(ascii_lookup(&json!({"code": 0xD800u64})).is_err(), "surrogate is invalid");
        assert!(ascii_lookup(&json!({"code": 0x110000u64})).is_err(), "past max code point");
        assert!(ascii_lookup(&json!({})).is_err());
    }

    #[test]
    fn planet_fact_known_planets() {
        let earth = planet_fact(&json!({"planet": "Earth"})).unwrap();
        assert!(earth.contains("planet #3"), "Earth is third from the Sun");
        assert!(earth.contains("1 known moons"));
        let mars = planet_fact(&json!({"planet": "mars"})).unwrap();
        assert!(mars.contains("planet #4"));
        assert!(mars.contains("2 known moons"));
        // Pluto is not a planet in the bundled table.
        assert!(planet_fact(&json!({"planet": "Pluto"})).is_err());
        assert!(planet_fact(&json!({})).is_err());
    }

    #[test]
    fn si_prefix_name_and_symbol() {
        assert_eq!(
            si_prefix(&json!({"prefix": "kilo"})).unwrap(),
            "kilo (k) = 10^3 = 1000"
        );
        // Symbol lookup, negative exponent rendered as a decimal.
        assert_eq!(
            si_prefix(&json!({"prefix": "m"})).unwrap(),
            "milli (m) = 10^-3 = 0.001"
        );
        // The µ sign and ASCII "u" both resolve to micro.
        assert_eq!(
            si_prefix(&json!({"prefix": "µ"})).unwrap(),
            "micro (µ) = 10^-6 = 1e-6"
        );
        assert_eq!(
            si_prefix(&json!({"prefix": "u"})).unwrap(),
            "micro (µ) = 10^-6 = 1e-6"
        );
        // Large exponent stays in 1e form.
        assert_eq!(
            si_prefix(&json!({"prefix": "giga"})).unwrap(),
            "giga (G) = 10^9 = 1e9"
        );
        assert!(si_prefix(&json!({"prefix": "zorp"})).is_err());
        assert!(si_prefix(&json!({})).is_err());
    }

    #[test]
    fn file_extension_lookup() {
        assert_eq!(
            file_extension(&json!({"ext": "json"})).unwrap(),
            ".json: data — JSON data."
        );
        // Leading dot + uppercase are tolerated.
        assert_eq!(
            file_extension(&json!({"ext": ".RS"})).unwrap(),
            ".rs: code — Rust source."
        );
        assert!(file_extension(&json!({"ext": "png"})).unwrap().contains("image"));
        assert!(file_extension(&json!({"ext": "zzz"})).is_err());
        assert!(file_extension(&json!({"ext": "."})).is_err());
        assert!(file_extension(&json!({})).is_err());
    }

    #[test]
    fn timezone_offset_fixed_offsets() {
        assert_eq!(
            timezone_offset(&json!({"abbr": "UTC"})).unwrap(),
            "UTC (Coordinated Universal Time) = UTC"
        );
        // Negative (west) offset.
        assert_eq!(
            timezone_offset(&json!({"abbr": "pst"})).unwrap(),
            "PST (Pacific Standard Time (North America)) = UTC-08:00"
        );
        // Half-hour offset.
        assert_eq!(
            timezone_offset(&json!({"abbr": "IST"})).unwrap(),
            "IST (India Standard Time) = UTC+05:30"
        );
        // Positive whole-hour.
        assert_eq!(
            timezone_offset(&json!({"abbr": "JST"})).unwrap(),
            "JST (Japan Standard Time) = UTC+09:00"
        );
        assert!(timezone_offset(&json!({"abbr": "ZZZ"})).is_err());
        assert!(timezone_offset(&json!({})).is_err());
    }

    #[test]
    fn every_run_is_deterministic() {
        // A spot-check that repeated calls give identical output (purity).
        let a = country_capital(&json!({"country": "Italy"})).unwrap();
        let b = country_capital(&json!({"country": "Italy"})).unwrap();
        assert_eq!(a, b);
        let c = http_status(&json!({"code": 200})).unwrap();
        let d = http_status(&json!({"code": 200})).unwrap();
        assert_eq!(c, d);
    }
}
