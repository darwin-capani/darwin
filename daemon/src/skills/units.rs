//! Category: UNITS — unit conversions with EXACT or well-known constant factors
//! (length, mass, temperature, speed, volume, area, data size) plus number-base,
//! roman-numeral, scientific-notation, fraction, angle, and fuel-economy
//! conversions. Every factor is a compile-time constant, so each skill is a pure,
//! total function of its args — no lookup table fetched at runtime, no network.
//!
//! Conversion style: where a chain of units shares one base unit (metres for
//! length, grams for mass, etc.) each unit carries its factor *to that base*, and
//! a convert is `value * from_factor / to_factor`. Factors are the SI/ISO exact
//! definitions (1 inch = 0.0254 m exactly, 1 lb = 453.59237 g exactly,
//! 1 mile = 1609.344 m exactly) so the results are correct, not approximations
//! that lie. Bad units / bad args return a friendly error; nothing panics and
//! nothing is fabricated.

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{Category, SkillDef};

/// The units catalog. The Library phase appended these `SkillDef::new(...)`
/// entries; mod.rs and the registry are untouched.
pub fn skills() -> Vec<SkillDef> {
    vec![
        SkillDef::new(
            "convert_length",
            Category::Units,
            "Convert a length between units (m, km, cm, mm, mi, yd, ft, in, nmi). Use for distance/height/length conversions.",
            &["convert length", "meters to feet", "miles to km", "how many inches", "cm to inches"],
            convert_length,
        ),
        SkillDef::new(
            "convert_mass",
            Category::Units,
            "Convert a mass/weight between units (kg, g, mg, t, lb, oz, st). Use for weight conversions.",
            &["convert weight", "kg to lb", "grams to ounces", "pounds to kilograms", "how many ounces"],
            convert_mass,
        ),
        SkillDef::new(
            "convert_temperature",
            Category::Units,
            "Convert a temperature between Celsius, Fahrenheit, and Kelvin. Use for temperature conversions.",
            &["convert temperature", "celsius to fahrenheit", "f to c", "kelvin", "how hot in fahrenheit"],
            convert_temperature,
        ),
        SkillDef::new(
            "convert_speed",
            Category::Units,
            "Convert a speed between units (m/s, km/h, mph, kn, ft/s). Use for velocity/pace conversions.",
            &["convert speed", "mph to kmh", "km/h to mph", "knots", "meters per second"],
            convert_speed,
        ),
        SkillDef::new(
            "convert_volume",
            Category::Units,
            "Convert a volume between units (l, ml, m3, gal, qt, pt, cup, floz, tbsp, tsp; US liquid). Use for liquid/cooking volume conversions.",
            &["convert volume", "liters to gallons", "cups to ml", "tablespoons", "fluid ounces"],
            convert_volume,
        ),
        SkillDef::new(
            "convert_area",
            Category::Units,
            "Convert an area between units (m2, km2, cm2, ha, acre, ft2, in2, mi2). Use for land/surface area conversions.",
            &["convert area", "square meters to square feet", "acres to hectares", "how many acres"],
            convert_area,
        ),
        SkillDef::new(
            "convert_data_size",
            Category::Units,
            "Convert a digital data size between units (B, KB/MB/GB/TB decimal SI, KiB/MiB/GiB/TiB binary, and bits b/Kib/Mib/Gib). Use for file-size/bandwidth conversions.",
            &["convert data size", "mb to gb", "gib to gb", "megabytes to bits", "kilobytes"],
            convert_data_size,
        ),
        SkillDef::new(
            "convert_number_base",
            Category::Units,
            "Convert a non-negative integer between bases 2..=36 (binary/octal/decimal/hex and more). Use for base/radix conversions.",
            &["convert base", "decimal to hex", "binary to decimal", "to hexadecimal", "octal", "radix"],
            convert_number_base,
        ),
        SkillDef::new(
            "roman_numeral",
            Category::Units,
            "Convert between an integer (1..=3999) and a Roman numeral, in either direction. Use for Roman-numeral encode/decode.",
            &["roman numeral", "to roman numerals", "what is XIV", "convert to roman", "MCMXCIV"],
            roman_numeral,
        ),
        SkillDef::new(
            "scientific_notation",
            Category::Units,
            "Convert a number to scientific notation (mantissa x 10^exp) or expand 'm e exp' back to a plain number. Use to express very large/small numbers compactly.",
            &["scientific notation", "standard form", "express in powers of ten", "expand 6.02e23"],
            scientific_notation,
        ),
        SkillDef::new(
            "fraction_decimal",
            Category::Units,
            "Convert a fraction to a decimal, or a decimal to a reduced fraction (lowest terms). Use for fraction<->decimal conversions.",
            &["fraction to decimal", "decimal to fraction", "simplify fraction", "what is 3/8 as a decimal"],
            fraction_decimal,
        ),
        SkillDef::new(
            "convert_angle",
            Category::Units,
            "Convert an angle between degrees, radians, and gradians. Use for angle-unit conversions.",
            &["convert angle", "degrees to radians", "radians to degrees", "gradians"],
            convert_angle,
        ),
        SkillDef::new(
            "fuel_economy",
            Category::Units,
            "Convert fuel economy between US mpg, UK mpg, and L/100km. Use for fuel-efficiency conversions.",
            &["fuel economy", "mpg to l/100km", "l/100km to mpg", "miles per gallon", "fuel efficiency"],
            fuel_economy,
        ),
    ]
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Read a required finite `f64` from `args[key]`. Rejects missing, non-numeric,
/// and NaN/infinite values with a friendly, skill-named error.
fn req_f64(args: &Value, key: &str, skill: &str) -> Result<f64> {
    let v = args
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow!("{skill} needs a numeric '{key}' argument"))?;
    if !v.is_finite() {
        return Err(anyhow!("{skill} '{key}' must be a finite number"));
    }
    Ok(v)
}

/// Read a required lowercased-and-trimmed string unit from `args[key]`.
fn req_unit(args: &Value, key: &str, skill: &str) -> Result<String> {
    let s = args
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{skill} needs a '{key}' unit string"))?;
    Ok(s.trim().to_ascii_lowercase())
}

/// Format an `f64` result without a trailing `.0` for whole numbers, and without
/// a stray "-0". Six significant-ish decimals then trimmed, ample for conversions.
fn fmt_num(x: f64) -> String {
    if x == 0.0 {
        return "0".to_string(); // collapse -0.0
    }
    // Up to 6 decimal places, trim trailing zeros and a bare trailing dot.
    let s = format!("{x:.6}");
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    trimmed.to_string()
}

/// Generic factor-table convert: look up `from` and `to` in `table` (unit ->
/// factor-to-base), then `value * from_factor / to_factor`. Unknown units list
/// the supported set so the error is actionable.
fn factor_convert(
    skill: &str,
    table: &[(&str, f64)],
    value: f64,
    from: &str,
    to: &str,
) -> Result<f64> {
    let lookup = |u: &str| -> Result<f64> {
        table
            .iter()
            .find(|(name, _)| *name == u)
            .map(|(_, f)| *f)
            .ok_or_else(|| {
                let units: Vec<&str> = table.iter().map(|(n, _)| *n).collect();
                anyhow!("{skill}: unknown unit '{u}' (supported: {})", units.join(", "))
            })
    };
    let ff = lookup(from)?;
    let tf = lookup(to)?;
    Ok(value * ff / tf)
}

/// gcd via Euclid, on u128 magnitudes. `gcd(0,0)=0`.
fn gcd_u128(a: u128, b: u128) -> u128 {
    let (mut a, mut b) = (a, b);
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

// ---------------------------------------------------------------------------
// Length
// ---------------------------------------------------------------------------

/// Length units -> metres (exact SI/imperial definitions).
const LENGTH: &[(&str, f64)] = &[
    ("m", 1.0),
    ("km", 1000.0),
    ("cm", 0.01),
    ("mm", 0.001),
    ("um", 1e-6),
    ("nm", 1e-9),
    ("mi", 1609.344),   // statute mile, exact
    ("yd", 0.9144),     // exact
    ("ft", 0.3048),     // exact
    ("in", 0.0254),     // exact
    ("nmi", 1852.0),    // nautical mile, exact
];

fn convert_length(args: &Value) -> Result<String> {
    let value = req_f64(args, "value", "convert_length")?;
    let from = req_unit(args, "from", "convert_length")?;
    let to = req_unit(args, "to", "convert_length")?;
    let out = factor_convert("convert_length", LENGTH, value, &from, &to)?;
    Ok(format!("{} {} = {} {}", fmt_num(value), from, fmt_num(out), to))
}

// ---------------------------------------------------------------------------
// Mass
// ---------------------------------------------------------------------------

/// Mass units -> grams (avoirdupois, exact definitions).
const MASS: &[(&str, f64)] = &[
    ("g", 1.0),
    ("kg", 1000.0),
    ("mg", 0.001),
    ("ug", 1e-6),
    ("t", 1_000_000.0),  // metric tonne
    ("lb", 453.59237),   // exact
    ("oz", 28.349523125),// exact (lb/16)
    ("st", 6350.29318),  // stone = 14 lb, exact
];

fn convert_mass(args: &Value) -> Result<String> {
    let value = req_f64(args, "value", "convert_mass")?;
    let from = req_unit(args, "from", "convert_mass")?;
    let to = req_unit(args, "to", "convert_mass")?;
    let out = factor_convert("convert_mass", MASS, value, &from, &to)?;
    Ok(format!("{} {} = {} {}", fmt_num(value), from, fmt_num(out), to))
}

// ---------------------------------------------------------------------------
// Temperature (affine, not a single factor)
// ---------------------------------------------------------------------------

/// Normalise a temperature unit alias to a canonical letter.
fn temp_unit(u: &str) -> Result<char> {
    match u {
        "c" | "celsius" | "centigrade" | "°c" => Ok('c'),
        "f" | "fahrenheit" | "°f" => Ok('f'),
        "k" | "kelvin" => Ok('k'),
        other => Err(anyhow!(
            "convert_temperature: unknown unit '{other}' (use c, f, or k)"
        )),
    }
}

fn convert_temperature(args: &Value) -> Result<String> {
    let value = req_f64(args, "value", "convert_temperature")?;
    let from = temp_unit(&req_unit(args, "from", "convert_temperature")?)?;
    let to = temp_unit(&req_unit(args, "to", "convert_temperature")?)?;
    // To Celsius first.
    let c = match from {
        'c' => value,
        'f' => (value - 32.0) * 5.0 / 9.0,
        'k' => value - 273.15,
        _ => unreachable!(),
    };
    // Reject below absolute zero (a real physical bound, friendly error).
    if c < -273.15 - 1e-9 {
        return Err(anyhow!(
            "convert_temperature: {} {} is below absolute zero",
            fmt_num(value),
            from
        ));
    }
    let out = match to {
        'c' => c,
        'f' => c * 9.0 / 5.0 + 32.0,
        'k' => c + 273.15,
        _ => unreachable!(),
    };
    Ok(format!(
        "{} °{} = {} °{}",
        fmt_num(value),
        from.to_ascii_uppercase(),
        fmt_num(out),
        to.to_ascii_uppercase()
    ))
}

// ---------------------------------------------------------------------------
// Speed
// ---------------------------------------------------------------------------

/// Speed units -> metres per second (exact).
const SPEED: &[(&str, f64)] = &[
    ("m/s", 1.0),
    ("mps", 1.0),
    ("km/h", 1000.0 / 3600.0),
    ("kmh", 1000.0 / 3600.0),
    ("kph", 1000.0 / 3600.0),
    ("mph", 1609.344 / 3600.0),
    ("kn", 1852.0 / 3600.0),  // knot
    ("knot", 1852.0 / 3600.0),
    ("ft/s", 0.3048),
    ("fps", 0.3048),
];

fn convert_speed(args: &Value) -> Result<String> {
    let value = req_f64(args, "value", "convert_speed")?;
    let from = req_unit(args, "from", "convert_speed")?;
    let to = req_unit(args, "to", "convert_speed")?;
    let out = factor_convert("convert_speed", SPEED, value, &from, &to)?;
    Ok(format!("{} {} = {} {}", fmt_num(value), from, fmt_num(out), to))
}

// ---------------------------------------------------------------------------
// Volume (US liquid)
// ---------------------------------------------------------------------------

/// Volume units -> litres. US liquid gallon = 3.785411784 L exactly; sub-units
/// derive from it. The metric units are exact.
const VOLUME: &[(&str, f64)] = &[
    ("l", 1.0),
    ("ml", 0.001),
    ("cl", 0.01),
    ("dl", 0.1),
    ("m3", 1000.0),
    ("cm3", 0.001),
    ("gal", 3.785411784),               // US liquid gallon, exact
    ("qt", 3.785411784 / 4.0),          // quart = gal/4
    ("pt", 3.785411784 / 8.0),          // pint = gal/8
    ("cup", 3.785411784 / 16.0),        // US legal-ish: gal/16 = 16 tbsp
    ("floz", 3.785411784 / 128.0),      // US fluid ounce = gal/128
    ("tbsp", 3.785411784 / 256.0),      // tablespoon = floz/2
    ("tsp", 3.785411784 / 768.0),       // teaspoon = tbsp/3
];

fn convert_volume(args: &Value) -> Result<String> {
    let value = req_f64(args, "value", "convert_volume")?;
    let from = req_unit(args, "from", "convert_volume")?;
    let to = req_unit(args, "to", "convert_volume")?;
    let out = factor_convert("convert_volume", VOLUME, value, &from, &to)?;
    Ok(format!("{} {} = {} {}", fmt_num(value), from, fmt_num(out), to))
}

// ---------------------------------------------------------------------------
// Area
// ---------------------------------------------------------------------------

/// Area units -> square metres. Imperial areas derive from the exact linear
/// definitions (1 ft = 0.3048 m -> 1 ft2 = 0.09290304 m2 exactly).
const AREA: &[(&str, f64)] = &[
    ("m2", 1.0),
    ("km2", 1_000_000.0),
    ("cm2", 0.0001),
    ("mm2", 0.000001),
    ("ha", 10_000.0),                  // hectare
    ("acre", 4046.8564224),            // exact (1 acre = 4840 yd2)
    ("ft2", 0.09290304),               // exact
    ("in2", 0.00064516),               // exact
    ("yd2", 0.83612736),               // exact
    ("mi2", 2_589_988.110336),         // exact (1609.344^2)
];

fn convert_area(args: &Value) -> Result<String> {
    let value = req_f64(args, "value", "convert_area")?;
    let from = req_unit(args, "from", "convert_area")?;
    let to = req_unit(args, "to", "convert_area")?;
    let out = factor_convert("convert_area", AREA, value, &from, &to)?;
    Ok(format!("{} {} = {} {}", fmt_num(value), from, fmt_num(out), to))
}

// ---------------------------------------------------------------------------
// Data size
// ---------------------------------------------------------------------------

/// Data-size units -> BYTES (bits map to fractional bytes; decimal SI vs binary
/// IEC are kept distinct so `gb` != `gib`).
const DATA: &[(&str, f64)] = &[
    ("b", 1.0 / 8.0),       // bit
    ("byte", 1.0),
    ("kb", 1e3),
    ("mb", 1e6),
    ("gb", 1e9),
    ("tb", 1e12),
    ("pb", 1e15),
    ("kib", 1024.0),
    ("mib", 1_048_576.0),                 // 1024^2
    ("gib", 1_073_741_824.0),             // 1024^3
    ("tib", 1_099_511_627_776.0),         // 1024^4
    ("pib", 1_125_899_906_842_624.0),     // 1024^5
    ("kbit", 1e3 / 8.0),
    ("mbit", 1e6 / 8.0),
    ("gbit", 1e9 / 8.0),
];

fn convert_data_size(args: &Value) -> Result<String> {
    let value = req_f64(args, "value", "convert_data_size")?;
    let from = req_unit(args, "from", "convert_data_size")?;
    let to = req_unit(args, "to", "convert_data_size")?;
    let out = factor_convert("convert_data_size", DATA, value, &from, &to)?;
    Ok(format!("{} {} = {} {}", fmt_num(value), from, fmt_num(out), to))
}

// ---------------------------------------------------------------------------
// Number base
// ---------------------------------------------------------------------------

fn convert_number_base(args: &Value) -> Result<String> {
    let s = args
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("convert_number_base needs a 'value' string (the digits)"))?
        .trim();
    let from = args
        .get("from_base")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("convert_number_base needs an integer 'from_base' (2..=36)"))?;
    let to = args
        .get("to_base")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("convert_number_base needs an integer 'to_base' (2..=36)"))?;
    if !(2..=36).contains(&from) || !(2..=36).contains(&to) {
        return Err(anyhow!("convert_number_base: bases must be 2..=36"));
    }
    if s.is_empty() {
        return Err(anyhow!("convert_number_base: 'value' is empty"));
    }
    // Parse the magnitude in `from` base (case-insensitive digits 0-9a-z).
    let n = u128::from_str_radix(&s.to_ascii_lowercase(), from as u32)
        .map_err(|_| anyhow!("convert_number_base: '{s}' is not a valid base-{from} integer"))?;
    let out = to_radix(n, to as u32);
    Ok(format!("{s} (base {from}) = {out} (base {to})"))
}

/// Render a u128 in `radix` (2..=36) as a lowercase string. `0` -> "0".
fn to_radix(mut n: u128, radix: u32) -> String {
    if n == 0 {
        return "0".to_string();
    }
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = Vec::new();
    let r = radix as u128;
    while n > 0 {
        buf.push(DIGITS[(n % r) as usize]);
        n /= r;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

// ---------------------------------------------------------------------------
// Roman numerals
// ---------------------------------------------------------------------------

const ROMAN: &[(u32, &str)] = &[
    (1000, "M"),
    (900, "CM"),
    (500, "D"),
    (400, "CD"),
    (100, "C"),
    (90, "XC"),
    (50, "L"),
    (40, "XL"),
    (10, "X"),
    (9, "IX"),
    (5, "V"),
    (4, "IV"),
    (1, "I"),
];

/// Integer -> Roman numeral (1..=3999). Pure + total over that range.
fn int_to_roman(mut n: u32) -> Result<String> {
    if !(1..=3999).contains(&n) {
        return Err(anyhow!("roman_numeral: integer must be 1..=3999, got {n}"));
    }
    let mut out = String::new();
    for &(v, sym) in ROMAN {
        while n >= v {
            out.push_str(sym);
            n -= v;
        }
    }
    Ok(out)
}

/// Roman numeral -> integer, validated by round-tripping the canonical form so
/// malformed input (IIII, IC, VV) is rejected rather than silently accepted.
fn roman_to_int(s: &str) -> Result<u32> {
    let up = s.trim().to_ascii_uppercase();
    if up.is_empty() {
        return Err(anyhow!("roman_numeral: empty numeral"));
    }
    let vals = |c: char| -> Result<u32> {
        Ok(match c {
            'I' => 1,
            'V' => 5,
            'X' => 10,
            'L' => 50,
            'C' => 100,
            'D' => 500,
            'M' => 1000,
            other => return Err(anyhow!("roman_numeral: '{other}' is not a Roman digit")),
        })
    };
    let chars: Vec<char> = up.chars().collect();
    // Evaluate the numeral by the subtractive rule, then validate by a canonical
    // round-trip so only the well-formed spelling of a value is accepted (IIII,
    // IC, VV all fail because they are not how int_to_roman renders that value).
    let value = roman_value_robust(&chars, &vals)?;
    if value == 0 || value > 3999 {
        return Err(anyhow!("roman_numeral: value out of range 1..=3999"));
    }
    if int_to_roman(value)? != up {
        return Err(anyhow!("roman_numeral: '{up}' is not a well-formed Roman numeral"));
    }
    Ok(value)
}

/// Robust subtractive-rule evaluation of a Roman numeral's digit values.
fn roman_value_robust(chars: &[char], vals: &dyn Fn(char) -> Result<u32>) -> Result<u32> {
    let mut total = 0u32;
    let mut i = 0;
    while i < chars.len() {
        let cur = vals(chars[i])?;
        let next = if i + 1 < chars.len() { vals(chars[i + 1])? } else { 0 };
        if cur < next {
            total += next - cur;
            i += 2;
        } else {
            total += cur;
            i += 1;
        }
    }
    Ok(total)
}

fn roman_numeral(args: &Value) -> Result<String> {
    // Direction is inferred from which arg is present.
    if let Some(n) = args.get("number").and_then(Value::as_u64) {
        let n = u32::try_from(n).map_err(|_| anyhow!("roman_numeral: number too large"))?;
        let roman = int_to_roman(n)?;
        return Ok(format!("{n} = {roman}"));
    }
    if let Some(s) = args.get("roman").and_then(Value::as_str) {
        let n = roman_to_int(s)?;
        return Ok(format!("{} = {n}", s.trim().to_ascii_uppercase()));
    }
    Err(anyhow!(
        "roman_numeral needs either a 'number' (1..=3999) or a 'roman' string"
    ))
}

// ---------------------------------------------------------------------------
// Scientific notation
// ---------------------------------------------------------------------------

fn scientific_notation(args: &Value) -> Result<String> {
    // Mode A: collapse a plain number to mantissa x 10^exp.
    if let Some(x) = args.get("value").and_then(Value::as_f64) {
        if !x.is_finite() {
            return Err(anyhow!("scientific_notation 'value' must be finite"));
        }
        if x == 0.0 {
            return Ok("0 = 0 x 10^0".to_string());
        }
        let exp = x.abs().log10().floor() as i32;
        let mantissa = x / 10f64.powi(exp);
        return Ok(format!(
            "{} = {} x 10^{}",
            fmt_num(x),
            fmt_num(mantissa),
            exp
        ));
    }
    // Mode B: expand mantissa + exp into a plain number.
    let m = args.get("mantissa").and_then(Value::as_f64);
    let e = args.get("exp").and_then(Value::as_i64);
    if let (Some(m), Some(e)) = (m, e) {
        if !m.is_finite() {
            return Err(anyhow!("scientific_notation 'mantissa' must be finite"));
        }
        if !(-308..=308).contains(&e) {
            return Err(anyhow!("scientific_notation 'exp' must be -308..=308"));
        }
        let x = m * 10f64.powi(e as i32);
        return Ok(format!("{} x 10^{} = {}", fmt_num(m), e, fmt_num(x)));
    }
    Err(anyhow!(
        "scientific_notation needs either a 'value' (to compress) or 'mantissa'+'exp' (to expand)"
    ))
}

// ---------------------------------------------------------------------------
// Fraction <-> decimal
// ---------------------------------------------------------------------------

fn fraction_decimal(args: &Value) -> Result<String> {
    // Mode A: fraction (numerator/denominator) -> decimal.
    let num = args.get("numerator").and_then(Value::as_i64);
    let den = args.get("denominator").and_then(Value::as_i64);
    if let (Some(n), Some(d)) = (num, den) {
        if d == 0 {
            return Err(anyhow!("fraction_decimal: denominator cannot be zero"));
        }
        let dec = n as f64 / d as f64;
        // Also present the reduced fraction for clarity.
        let (rn, rd) = reduce_fraction(n, d);
        return Ok(format!("{n}/{d} = {} (reduced {rn}/{rd})", fmt_num(dec)));
    }
    // Mode B: decimal -> reduced fraction.
    if let Some(x) = args.get("decimal").and_then(Value::as_f64) {
        if !x.is_finite() {
            return Err(anyhow!("fraction_decimal 'decimal' must be finite"));
        }
        let (n, d) = decimal_to_fraction(x)?;
        return Ok(format!("{} = {n}/{d}", fmt_num(x)));
    }
    Err(anyhow!(
        "fraction_decimal needs either 'numerator'+'denominator' or a 'decimal'"
    ))
}

/// Reduce a signed fraction to lowest terms, sign carried on the numerator,
/// denominator kept positive.
fn reduce_fraction(n: i64, d: i64) -> (i64, i64) {
    if n == 0 {
        return (0, 1);
    }
    let g = gcd_u128(n.unsigned_abs() as u128, d.unsigned_abs() as u128).max(1) as i64;
    let mut rn = n / g;
    let mut rd = d / g;
    if rd < 0 {
        rn = -rn;
        rd = -rd;
    }
    (rn, rd)
}

/// Convert a finite decimal with up to 9 fractional digits to a reduced
/// fraction exactly (it is treated as value * 10^k / 10^k). Decimals with more
/// precision are rounded to 9 places — bounded + honest, never an infinite loop.
fn decimal_to_fraction(x: f64) -> Result<(i64, i64)> {
    if x == 0.0 {
        return Ok((0, 1));
    }
    const SCALE: i64 = 1_000_000_000; // 9 dp
    let scaled = (x * SCALE as f64).round();
    if !scaled.is_finite() || scaled.abs() > i64::MAX as f64 {
        return Err(anyhow!("fraction_decimal: number too large to convert"));
    }
    let n = scaled as i64;
    Ok(reduce_fraction(n, SCALE))
}

// ---------------------------------------------------------------------------
// Angle
// ---------------------------------------------------------------------------

/// Angle units -> radians.
const ANGLE: &[(&str, f64)] = &[
    ("rad", 1.0),
    ("radian", 1.0),
    ("radians", 1.0),
    ("deg", std::f64::consts::PI / 180.0),
    ("degree", std::f64::consts::PI / 180.0),
    ("degrees", std::f64::consts::PI / 180.0),
    ("grad", std::f64::consts::PI / 200.0),
    ("gradian", std::f64::consts::PI / 200.0),
    ("gon", std::f64::consts::PI / 200.0),
];

fn convert_angle(args: &Value) -> Result<String> {
    let value = req_f64(args, "value", "convert_angle")?;
    let from = req_unit(args, "from", "convert_angle")?;
    let to = req_unit(args, "to", "convert_angle")?;
    let out = factor_convert("convert_angle", ANGLE, value, &from, &to)?;
    Ok(format!("{} {} = {} {}", fmt_num(value), from, fmt_num(out), to))
}

// ---------------------------------------------------------------------------
// Fuel economy
// ---------------------------------------------------------------------------

/// Fuel-economy conversion. Internally everything goes through L/100km, the only
/// linear "consumption" unit; mpg is an inverse unit so we convert via the exact
/// gallon/mile definitions. US gallon = 3.785411784 L, UK gallon = 4.54609 L,
/// mile = 1.609344 km.
fn fuel_economy(args: &Value) -> Result<String> {
    let value = req_f64(args, "value", "fuel_economy")?;
    let from = req_unit(args, "from", "fuel_economy")?;
    let to = req_unit(args, "to", "fuel_economy")?;
    if value <= 0.0 {
        return Err(anyhow!("fuel_economy 'value' must be positive"));
    }

    // Litres of fuel per 100 km is the canonical internal representation.
    let to_l100 = |unit: &str, v: f64| -> Result<f64> {
        match unit {
            "l/100km" | "l100km" | "lp100km" => Ok(v),
            // mpg (US): miles per US gallon. L/100km = (100 * gal_L) / (mpg * mi_km)
            "mpg" | "mpg_us" | "us_mpg" => Ok(100.0 * 3.785411784 / (v * 1.609344)),
            "mpg_uk" | "uk_mpg" | "mpg_imp" => Ok(100.0 * 4.54609 / (v * 1.609344)),
            other => Err(anyhow!(
                "fuel_economy: unknown unit '{other}' (use l/100km, mpg, or mpg_uk)"
            )),
        }
    };
    let from_l100 = |unit: &str, l100: f64| -> Result<f64> {
        match unit {
            "l/100km" | "l100km" | "lp100km" => Ok(l100),
            "mpg" | "mpg_us" | "us_mpg" => Ok(100.0 * 3.785411784 / (l100 * 1.609344)),
            "mpg_uk" | "uk_mpg" | "mpg_imp" => Ok(100.0 * 4.54609 / (l100 * 1.609344)),
            other => Err(anyhow!(
                "fuel_economy: unknown unit '{other}' (use l/100km, mpg, or mpg_uk)"
            )),
        }
    };

    let l100 = to_l100(&from, value)?;
    let out = from_l100(&to, l100)?;
    Ok(format!("{} {} = {} {}", fmt_num(value), from, fmt_num(out), to))
}

// ===========================================================================
// Tests — known-answer vectors + error/edge cases for every skill.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Parse the numeric result out of a "... = N unit" output string.
    fn out_num(s: &str) -> f64 {
        // The result number is the token right after " = ".
        let rhs = s.split(" = ").nth(1).unwrap();
        rhs.split_whitespace().next().unwrap().parse().unwrap()
    }

    fn approx(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    // ----- catalog ----------------------------------------------------------
    #[test]
    fn catalog_has_thirteen_pure_units_skills() {
        let s = skills();
        assert_eq!(s.len(), 13, "thirteen units skills ship");
        assert!(
            s.iter().all(|d| !d.consequential && !d.source_gated),
            "every units skill is pure read-only"
        );
        assert!(
            s.iter().all(|d| d.category == Category::Units),
            "all in the Units category"
        );
        // Names are unique within the file.
        let mut names: Vec<&str> = s.iter().map(|d| d.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 13, "no duplicate names");
    }

    // ----- length -----------------------------------------------------------
    #[test]
    fn length_known_vectors() {
        // 1 mile = 1609.344 m exactly.
        let o = convert_length(&json!({"value": 1, "from": "mi", "to": "m"})).unwrap();
        assert!(approx(out_num(&o), 1609.344, 1e-6), "{o}");
        // 1 foot = 12 inches.
        let o = convert_length(&json!({"value": 1, "from": "ft", "to": "in"})).unwrap();
        assert!(approx(out_num(&o), 12.0, 1e-9), "{o}");
        // 2.54 cm = 1 inch.
        let o = convert_length(&json!({"value": 2.54, "from": "cm", "to": "in"})).unwrap();
        assert!(approx(out_num(&o), 1.0, 1e-9), "{o}");
    }

    #[test]
    fn length_errors() {
        assert!(convert_length(&json!({"value": 1, "from": "lightyear", "to": "m"})).is_err());
        assert!(convert_length(&json!({"from": "m", "to": "ft"})).is_err(), "missing value");
        assert!(convert_length(&json!({"value": 1, "to": "ft"})).is_err(), "missing from");
    }

    // ----- mass -------------------------------------------------------------
    #[test]
    fn mass_known_vectors() {
        // 1 kg = 1000 g.
        let o = convert_mass(&json!({"value": 1, "from": "kg", "to": "g"})).unwrap();
        assert!(approx(out_num(&o), 1000.0, 1e-9), "{o}");
        // 1 lb = 16 oz.
        let o = convert_mass(&json!({"value": 1, "from": "lb", "to": "oz"})).unwrap();
        assert!(approx(out_num(&o), 16.0, 1e-9), "{o}");
        // 1 kg = 2.2046226218... lb.
        let o = convert_mass(&json!({"value": 1, "from": "kg", "to": "lb"})).unwrap();
        assert!(approx(out_num(&o), 2.2046226218, 1e-6), "{o}");
        // 1 stone = 14 lb.
        let o = convert_mass(&json!({"value": 1, "from": "st", "to": "lb"})).unwrap();
        assert!(approx(out_num(&o), 14.0, 1e-6), "{o}");
    }

    #[test]
    fn mass_errors() {
        assert!(convert_mass(&json!({"value": 1, "from": "carat", "to": "g"})).is_err());
        assert!(convert_mass(&json!({"value": "heavy", "from": "g", "to": "kg"})).is_err());
    }

    // ----- temperature ------------------------------------------------------
    #[test]
    fn temperature_known_vectors() {
        let o = convert_temperature(&json!({"value": 100, "from": "c", "to": "f"})).unwrap();
        assert!(approx(out_num(&o), 212.0, 1e-9), "{o}");
        let o = convert_temperature(&json!({"value": 32, "from": "f", "to": "c"})).unwrap();
        assert!(approx(out_num(&o), 0.0, 1e-9), "{o}");
        let o = convert_temperature(&json!({"value": 0, "from": "c", "to": "k"})).unwrap();
        assert!(approx(out_num(&o), 273.15, 1e-9), "{o}");
        let o = convert_temperature(&json!({"value": -40, "from": "c", "to": "f"})).unwrap();
        assert!(approx(out_num(&o), -40.0, 1e-9), "{o} (the -40 crossover)");
    }

    #[test]
    fn temperature_rejects_below_absolute_zero_and_bad_units() {
        assert!(
            convert_temperature(&json!({"value": -300, "from": "c", "to": "k"})).is_err(),
            "below absolute zero"
        );
        assert!(convert_temperature(&json!({"value": 0, "from": "x", "to": "c"})).is_err());
    }

    // ----- speed ------------------------------------------------------------
    #[test]
    fn speed_known_vectors() {
        // 1 m/s = 3.6 km/h.
        let o = convert_speed(&json!({"value": 1, "from": "m/s", "to": "km/h"})).unwrap();
        assert!(approx(out_num(&o), 3.6, 1e-9), "{o}");
        // 60 mph = 96.56064 km/h.
        let o = convert_speed(&json!({"value": 60, "from": "mph", "to": "km/h"})).unwrap();
        assert!(approx(out_num(&o), 96.56064, 1e-5), "{o}");
        // 1 knot = 1.852 km/h.
        let o = convert_speed(&json!({"value": 1, "from": "kn", "to": "km/h"})).unwrap();
        assert!(approx(out_num(&o), 1.852, 1e-9), "{o}");
    }

    #[test]
    fn speed_errors() {
        assert!(convert_speed(&json!({"value": 1, "from": "warp", "to": "mph"})).is_err());
    }

    // ----- volume -----------------------------------------------------------
    #[test]
    fn volume_known_vectors() {
        // 1 US gallon = 3.785411784 L.
        let o = convert_volume(&json!({"value": 1, "from": "gal", "to": "l"})).unwrap();
        assert!(approx(out_num(&o), 3.785411784, 1e-6), "{o}");
        // 1 gallon = 16 cups.
        let o = convert_volume(&json!({"value": 1, "from": "gal", "to": "cup"})).unwrap();
        assert!(approx(out_num(&o), 16.0, 1e-6), "{o}");
        // 1 tablespoon = 3 teaspoons.
        let o = convert_volume(&json!({"value": 1, "from": "tbsp", "to": "tsp"})).unwrap();
        assert!(approx(out_num(&o), 3.0, 1e-6), "{o}");
        // 1 L = 1000 mL.
        let o = convert_volume(&json!({"value": 1, "from": "l", "to": "ml"})).unwrap();
        assert!(approx(out_num(&o), 1000.0, 1e-9), "{o}");
    }

    #[test]
    fn volume_errors() {
        assert!(convert_volume(&json!({"value": 1, "from": "barrel", "to": "l"})).is_err());
    }

    // ----- area -------------------------------------------------------------
    #[test]
    fn area_known_vectors() {
        // 1 hectare = 10000 m2.
        let o = convert_area(&json!({"value": 1, "from": "ha", "to": "m2"})).unwrap();
        assert!(approx(out_num(&o), 10000.0, 1e-6), "{o}");
        // 1 acre = 4046.8564224 m2.
        let o = convert_area(&json!({"value": 1, "from": "acre", "to": "m2"})).unwrap();
        assert!(approx(out_num(&o), 4046.8564224, 1e-4), "{o}");
        // 1 ft2 = 144 in2.
        let o = convert_area(&json!({"value": 1, "from": "ft2", "to": "in2"})).unwrap();
        assert!(approx(out_num(&o), 144.0, 1e-6), "{o}");
    }

    #[test]
    fn area_errors() {
        assert!(convert_area(&json!({"value": 1, "from": "rood", "to": "m2"})).is_err());
    }

    // ----- data size --------------------------------------------------------
    #[test]
    fn data_size_known_vectors() {
        // 1 GB (decimal) = 1000 MB.
        let o = convert_data_size(&json!({"value": 1, "from": "gb", "to": "mb"})).unwrap();
        assert!(approx(out_num(&o), 1000.0, 1e-6), "{o}");
        // 1 GiB = 1024 MiB.
        let o = convert_data_size(&json!({"value": 1, "from": "gib", "to": "mib"})).unwrap();
        assert!(approx(out_num(&o), 1024.0, 1e-6), "{o}");
        // 1 byte = 8 bits.
        let o = convert_data_size(&json!({"value": 1, "from": "byte", "to": "b"})).unwrap();
        assert!(approx(out_num(&o), 8.0, 1e-9), "{o}");
        // 1 GiB = 1073741824 bytes.
        let o = convert_data_size(&json!({"value": 1, "from": "gib", "to": "byte"})).unwrap();
        assert!(approx(out_num(&o), 1_073_741_824.0, 1.0), "{o}");
        // Decimal != binary: 1 GB in GiB is < 1.
        let o = convert_data_size(&json!({"value": 1, "from": "gb", "to": "gib"})).unwrap();
        assert!(out_num(&o) < 1.0 && out_num(&o) > 0.9, "{o}");
    }

    #[test]
    fn data_size_errors() {
        assert!(convert_data_size(&json!({"value": 1, "from": "nibble", "to": "byte"})).is_err());
    }

    // ----- number base ------------------------------------------------------
    #[test]
    fn number_base_known_vectors() {
        // 255 decimal = ff hex.
        let o = convert_number_base(&json!({"value": "255", "from_base": 10, "to_base": 16})).unwrap();
        assert!(o.contains("ff"), "{o}");
        // 1010 binary = 10 decimal.
        let o = convert_number_base(&json!({"value": "1010", "from_base": 2, "to_base": 10})).unwrap();
        assert!(o.contains("10 (base 10)"), "{o}");
        // ff hex = 11111111 binary.
        let o = convert_number_base(&json!({"value": "ff", "from_base": 16, "to_base": 2})).unwrap();
        assert!(o.contains("11111111"), "{o}");
        // 0 stays 0 in any base.
        let o = convert_number_base(&json!({"value": "0", "from_base": 10, "to_base": 2})).unwrap();
        assert!(o.contains("0 (base 2)"), "{o}");
    }

    #[test]
    fn number_base_errors() {
        // 'z' is not a base-10 digit.
        assert!(convert_number_base(&json!({"value": "z", "from_base": 10, "to_base": 2})).is_err());
        // base out of range.
        assert!(convert_number_base(&json!({"value": "1", "from_base": 37, "to_base": 2})).is_err());
        assert!(convert_number_base(&json!({"value": "1", "from_base": 1, "to_base": 2})).is_err());
        assert!(convert_number_base(&json!({"value": "", "from_base": 10, "to_base": 2})).is_err());
    }

    // ----- roman numerals ---------------------------------------------------
    #[test]
    fn roman_known_vectors_and_roundtrip() {
        assert_eq!(int_to_roman(4).unwrap(), "IV");
        assert_eq!(int_to_roman(9).unwrap(), "IX");
        assert_eq!(int_to_roman(14).unwrap(), "XIV");
        assert_eq!(int_to_roman(40).unwrap(), "XL");
        assert_eq!(int_to_roman(1994).unwrap(), "MCMXCIV");
        assert_eq!(int_to_roman(3999).unwrap(), "MMMCMXCIX");
        assert_eq!(roman_to_int("MCMXCIV").unwrap(), 1994);
        assert_eq!(roman_to_int("xiv").unwrap(), 14);
        // Full round-trip across the whole valid range.
        for n in 1..=3999u32 {
            let r = int_to_roman(n).unwrap();
            assert_eq!(roman_to_int(&r).unwrap(), n, "roundtrip {n} -> {r}");
        }
    }

    #[test]
    fn roman_skill_both_directions_and_errors() {
        let o = roman_numeral(&json!({"number": 2024})).unwrap();
        assert!(o.contains("MMXXIV"), "{o}");
        let o = roman_numeral(&json!({"roman": "XIV"})).unwrap();
        assert!(o.contains("= 14"), "{o}");
        // Out of range.
        assert!(roman_numeral(&json!({"number": 0})).is_err());
        assert!(roman_numeral(&json!({"number": 4000})).is_err());
        // Malformed numerals rejected (non-canonical / illegal).
        assert!(roman_to_int("IIII").is_err(), "IIII is not canonical");
        assert!(roman_to_int("IC").is_err(), "IC is not a valid subtractive");
        assert!(roman_to_int("VV").is_err(), "VV is illegal");
        assert!(roman_to_int("ABC").is_err(), "non-Roman letters");
        // Neither arg present.
        assert!(roman_numeral(&json!({})).is_err());
    }

    // ----- scientific notation ----------------------------------------------
    #[test]
    fn scientific_notation_compress_and_expand() {
        // Compress.
        let o = scientific_notation(&json!({"value": 602214000000000000000000.0_f64})).unwrap();
        assert!(o.contains("x 10^23"), "{o}");
        let o = scientific_notation(&json!({"value": 0.0042})).unwrap();
        assert!(o.contains("10^-3"), "{o}");
        let o = scientific_notation(&json!({"value": 0})).unwrap();
        assert!(o.contains("0 x 10^0"), "{o}");
        // Expand: 6.022 x 10^3 = 6022.
        let o = scientific_notation(&json!({"mantissa": 6.022, "exp": 3})).unwrap();
        assert!(o.ends_with("= 6022"), "{o}");
    }

    #[test]
    fn scientific_notation_errors() {
        assert!(scientific_notation(&json!({})).is_err(), "needs value or mantissa+exp");
        assert!(
            scientific_notation(&json!({"mantissa": 1.0, "exp": 9999})).is_err(),
            "exp out of range"
        );
    }

    // ----- fraction <-> decimal --------------------------------------------
    #[test]
    fn fraction_to_decimal_and_back() {
        // 3/8 = 0.375.
        let o = fraction_decimal(&json!({"numerator": 3, "denominator": 8})).unwrap();
        assert!(o.contains("0.375"), "{o}");
        // 6/8 reduces to 3/4.
        let o = fraction_decimal(&json!({"numerator": 6, "denominator": 8})).unwrap();
        assert!(o.contains("reduced 3/4"), "{o}");
        // 0.75 -> 3/4.
        let o = fraction_decimal(&json!({"decimal": 0.75})).unwrap();
        assert!(o.ends_with("= 3/4"), "{o}");
        // 0.5 -> 1/2.
        let o = fraction_decimal(&json!({"decimal": 0.5})).unwrap();
        assert!(o.ends_with("= 1/2"), "{o}");
    }

    #[test]
    fn fraction_decimal_errors_and_edges() {
        assert!(
            fraction_decimal(&json!({"numerator": 1, "denominator": 0})).is_err(),
            "div by zero"
        );
        assert!(fraction_decimal(&json!({})).is_err(), "needs one mode");
        // 0 -> 0/1 reduced.
        let o = fraction_decimal(&json!({"decimal": 0})).unwrap();
        assert!(o.ends_with("= 0/1"), "{o}");
    }

    // ----- angle ------------------------------------------------------------
    #[test]
    fn angle_known_vectors() {
        // 180 deg = pi rad. (out_num parses the 6-dp formatted string, so the
        // tolerance reflects that display precision, not the f64 math precision.)
        let o = convert_angle(&json!({"value": 180, "from": "deg", "to": "rad"})).unwrap();
        assert!(approx(out_num(&o), std::f64::consts::PI, 1e-5), "{o}");
        // pi rad = 180 deg.
        let o = convert_angle(&json!({"value": std::f64::consts::PI, "from": "rad", "to": "deg"})).unwrap();
        assert!(approx(out_num(&o), 180.0, 1e-4), "{o}");
        // 200 grad = 180 deg.
        let o = convert_angle(&json!({"value": 200, "from": "grad", "to": "deg"})).unwrap();
        assert!(approx(out_num(&o), 180.0, 1e-6), "{o}");
    }

    #[test]
    fn angle_errors() {
        assert!(convert_angle(&json!({"value": 1, "from": "turn", "to": "deg"})).is_err());
    }

    // ----- fuel economy -----------------------------------------------------
    #[test]
    fn fuel_economy_known_vectors() {
        // 30 US mpg ~= 7.84 L/100km.
        let o = fuel_economy(&json!({"value": 30, "from": "mpg", "to": "l/100km"})).unwrap();
        assert!(approx(out_num(&o), 7.8408, 1e-3), "{o}");
        // Round trip: L/100km back to mpg.
        let o = fuel_economy(&json!({"value": 7.8408, "from": "l/100km", "to": "mpg"})).unwrap();
        assert!(approx(out_num(&o), 30.0, 1e-2), "{o}");
        // UK mpg is larger than US mpg for the same consumption.
        let us = out_num(&fuel_economy(&json!({"value": 8.0, "from": "l/100km", "to": "mpg"})).unwrap());
        let uk = out_num(&fuel_economy(&json!({"value": 8.0, "from": "l/100km", "to": "mpg_uk"})).unwrap());
        assert!(uk > us, "UK gallon is bigger: uk={uk} us={us}");
    }

    #[test]
    fn fuel_economy_errors() {
        assert!(fuel_economy(&json!({"value": 0, "from": "mpg", "to": "l/100km"})).is_err());
        assert!(fuel_economy(&json!({"value": 30, "from": "kpl", "to": "mpg"})).is_err());
    }

    // ----- determinism ------------------------------------------------------
    #[test]
    fn conversions_are_deterministic() {
        let a = convert_length(&json!({"value": 5, "from": "km", "to": "mi"})).unwrap();
        let b = convert_length(&json!({"value": 5, "from": "km", "to": "mi"})).unwrap();
        assert_eq!(a, b, "same input -> same output");
    }
}
