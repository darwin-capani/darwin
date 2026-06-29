//! Category: DATETIME — calendar/clock arithmetic over INJECTED instants. A
//! skill here NEVER reads the wall clock: any "now" comes in as an argument (an
//! ISO date/timestamp or epoch seconds) so the skill stays pure + hermetically
//! testable. Day-of-week, date diffs, add/sub days, leap-year, week-of-year,
//! unix<->iso, age, days-until, duration humanize, cron explain, fixed-offset
//! timezone convert.
//!
//! The civil-date math is hand-rolled from Howard Hinnant's well-known
//! `days_from_civil` / `civil_from_days` algorithms (proleptic Gregorian
//! calendar, exact for any year — no approximations, no `chrono`-clock). Every
//! function is total over its bounded, validated input and returns a friendly
//! error (never panics, never fabricates) on bad args.

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{Category, SkillDef};

/// The datetime catalog. The Library phase appended these `SkillDef::new(...)`
/// entries; mod.rs and the registry are untouched.
pub fn skills() -> Vec<SkillDef> {
    vec![
        SkillDef::new(
            "date_add",
            Category::Datetime,
            "Add or subtract a number of days from a date. Use for 'what's the date N days from <date>' or scheduling math; pass a negative count to go backwards.",
            &["days from", "add days", "subtract days", "N days later", "days ago from a date", "date plus days"],
            date_add,
        ),
        SkillDef::new(
            "weekday_of",
            Category::Datetime,
            "Name the weekday (Monday..Sunday) of a given calendar date. Use for 'what day of the week is/was <date>'.",
            &["what day of the week", "weekday of", "which day is", "day of week"],
            weekday_of,
        ),
        SkillDef::new(
            "days_between",
            Category::Datetime,
            "Count whole days between two calendar dates (signed: later minus earlier). Use for 'how many days between A and B' or date spans.",
            &["days between", "how many days from", "difference between dates", "date span"],
            days_between,
        ),
        SkillDef::new(
            "days_until",
            Category::Datetime,
            "Days from an injected 'today' until a target date (signed; negative if the target is in the past). Use for countdowns; pass today's date explicitly.",
            &["days until", "countdown to", "how many days left", "days remaining"],
            days_until,
        ),
        SkillDef::new(
            "age_from_birthdate",
            Category::Datetime,
            "Whole-years age on an injected reference date, given a birthdate. Use for 'how old is someone born on <date> as of <date>'.",
            &["how old", "age from birthdate", "calculate age", "years old"],
            age_from_birthdate,
        ),
        SkillDef::new(
            "leap_year",
            Category::Datetime,
            "Tell whether a year is a leap year (proleptic Gregorian rule). Use for 'is <year> a leap year' or to know February's length.",
            &["leap year", "is it a leap year", "366 days", "february 29"],
            leap_year,
        ),
        SkillDef::new(
            "week_of_year",
            Category::Datetime,
            "ISO-8601 week number (and ISO week-year) of a date. Use for 'what ISO week is <date>' or week-numbered planning.",
            &["week of year", "iso week", "week number", "what week is"],
            week_of_year,
        ),
        SkillDef::new(
            "unix_to_iso",
            Category::Datetime,
            "Convert a Unix epoch-seconds timestamp (UTC) to an ISO-8601 'YYYY-MM-DDTHH:MM:SSZ' string. Use to make an epoch value human-readable.",
            &["unix to iso", "epoch to date", "timestamp to date", "convert epoch seconds"],
            unix_to_iso,
        ),
        SkillDef::new(
            "iso_to_unix",
            Category::Datetime,
            "Convert an ISO-8601 UTC datetime ('YYYY-MM-DDTHH:MM:SSZ' or a bare date) to Unix epoch seconds. Use to get a numeric timestamp from a date.",
            &["iso to unix", "date to epoch", "date to timestamp", "convert to epoch seconds"],
            iso_to_unix,
        ),
        SkillDef::new(
            "duration_humanize",
            Category::Datetime,
            "Render a duration in seconds as a readable 'Xd Yh Zm Ws' string. Use to humanize an elapsed/remaining number of seconds.",
            &["humanize duration", "seconds to days hours", "format duration", "how long is N seconds"],
            duration_humanize,
        ),
        SkillDef::new(
            "tz_convert",
            Category::Datetime,
            "Shift a wall-clock datetime by a FIXED UTC offset (e.g. -05:00 to +09:00). No timezone database / no DST — pass explicit offsets in minutes or ±HH:MM.",
            &["timezone convert", "change utc offset", "convert time zone offset", "shift hours"],
            tz_convert,
        ),
        SkillDef::new(
            "cron_explain",
            Category::Datetime,
            "Explain a standard 5-field cron expression (min hour dom month dow) in plain English. Use to read a crontab line; supports *, lists, ranges and step values.",
            &["explain cron", "what does this cron mean", "read crontab", "cron schedule"],
            cron_explain,
        ),
    ]
}

// ---------------------------------------------------------------------------
// Core civil-date algorithms (Howard Hinnant, proleptic Gregorian). Pure.
// `days_from_civil` maps a (y, m, d) to a day count relative to 1970-01-01
// (which is day 0); `civil_from_days` is its exact inverse. Both are valid for
// the entire range we accept and carry no clock.
// ---------------------------------------------------------------------------

/// Serial day number of a civil date relative to the Unix epoch (1970-01-01 = 0).
/// Exact for any proleptic-Gregorian date. `m` in 1..=12, `d` in 1..=31.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`]: serial day number -> (year, month, day).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Is `year` a leap year under the proleptic Gregorian rule?
fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Number of days in a given month of a given year (1..=12).
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Accepted year range — wide enough for any realistic date, bounded so input is
/// not unbounded and arithmetic never overflows.
const YEAR_MIN: i64 = 1;
const YEAR_MAX: i64 = 9999;

/// Parse + VALIDATE a `YYYY-MM-DD` date into (y, m, d). Rejects out-of-range
/// fields and impossible dates (e.g. 2021-02-29) with a friendly error.
fn parse_date(s: &str) -> Result<(i64, i64, i64)> {
    let s = s.trim();
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return Err(anyhow!("date '{s}' must be in YYYY-MM-DD form"));
    }
    let y: i64 = parts[0]
        .parse()
        .map_err(|_| anyhow!("date '{s}': year is not a number"))?;
    let m: i64 = parts[1]
        .parse()
        .map_err(|_| anyhow!("date '{s}': month is not a number"))?;
    let d: i64 = parts[2]
        .parse()
        .map_err(|_| anyhow!("date '{s}': day is not a number"))?;
    if !(YEAR_MIN..=YEAR_MAX).contains(&y) {
        return Err(anyhow!("date '{s}': year must be {YEAR_MIN}..={YEAR_MAX}"));
    }
    if !(1..=12).contains(&m) {
        return Err(anyhow!("date '{s}': month must be 1..=12"));
    }
    let dim = days_in_month(y, m);
    if !(1..=dim).contains(&d) {
        return Err(anyhow!("date '{s}': day must be 1..={dim} for that month/year"));
    }
    Ok((y, m, d))
}

/// Format (y, m, d) as a zero-padded `YYYY-MM-DD` string.
fn fmt_date(y: i64, m: i64, d: i64) -> String {
    format!("{y:04}-{m:02}-{d:02}")
}

/// Read a required string arg, or a friendly error naming the skill + key.
fn req_str<'a>(args: &'a Value, key: &str, skill: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{skill} needs a '{key}' string argument"))
}

/// Read a required date arg by key (validated).
fn req_date(args: &Value, key: &str, skill: &str) -> Result<(i64, i64, i64)> {
    parse_date(req_str(args, key, skill)?)
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

/// `date_add {date, days}` -> the date shifted by `days` (may be negative).
fn date_add(args: &Value) -> Result<String> {
    let (y, m, d) = req_date(args, "date", "date_add")?;
    let days = args
        .get("days")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("date_add needs an integer 'days' argument (negative to subtract)"))?;
    if !(-3_650_000..=3_650_000).contains(&days) {
        return Err(anyhow!("date_add 'days' must be within ±3,650,000"));
    }
    let serial = days_from_civil(y, m, d) + days;
    let (ny, nm, nd) = civil_from_days(serial);
    if !(YEAR_MIN..=YEAR_MAX).contains(&ny) {
        return Err(anyhow!("date_add result year {ny} is outside {YEAR_MIN}..={YEAR_MAX}"));
    }
    let verb = if days >= 0 { "after" } else { "before" };
    Ok(format!(
        "{} is {} days {} {}",
        fmt_date(ny, nm, nd),
        days.abs(),
        verb,
        fmt_date(y, m, d)
    ))
}

/// Weekday name for a serial day number. 1970-01-01 was a Thursday.
const WEEKDAYS: [&str; 7] = [
    "Thursday",  // serial 0 = 1970-01-01
    "Friday",
    "Saturday",
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
];

/// Weekday name (Monday..Sunday) for a (y, m, d).
fn weekday_name(y: i64, m: i64, d: i64) -> &'static str {
    let serial = days_from_civil(y, m, d);
    WEEKDAYS[serial.rem_euclid(7) as usize]
}

/// `weekday_of {date}` -> the weekday name of that date.
fn weekday_of(args: &Value) -> Result<String> {
    let (y, m, d) = req_date(args, "date", "weekday_of")?;
    Ok(format!("{} is a {}", fmt_date(y, m, d), weekday_name(y, m, d)))
}

/// `days_between {from, to}` -> signed whole days (to - from).
fn days_between(args: &Value) -> Result<String> {
    let (fy, fm, fd) = req_date(args, "from", "days_between")?;
    let (ty, tm, td) = req_date(args, "to", "days_between")?;
    let diff = days_from_civil(ty, tm, td) - days_from_civil(fy, fm, fd);
    Ok(format!(
        "{} to {}: {} day{}",
        fmt_date(fy, fm, fd),
        fmt_date(ty, tm, td),
        diff,
        if diff.abs() == 1 { "" } else { "s" }
    ))
}

/// `days_until {today, target}` -> signed days (target - today). `today` is
/// INJECTED (never the wall clock) so the skill is deterministic.
fn days_until(args: &Value) -> Result<String> {
    let (cy, cm, cd) = req_date(args, "today", "days_until")?;
    let (ty, tm, td) = req_date(args, "target", "days_until")?;
    let diff = days_from_civil(ty, tm, td) - days_from_civil(cy, cm, cd);
    let phrase = match diff {
        0 => "is today".to_string(),
        d if d > 0 => format!("is in {} day{}", d, if d == 1 { "" } else { "s" }),
        d => format!("was {} day{} ago", -d, if d == -1 { "" } else { "s" }),
    };
    Ok(format!("{} {}", fmt_date(ty, tm, td), phrase))
}

/// `age_from_birthdate {birthdate, on}` -> whole years old on the `on` date.
/// `on` is INJECTED (never the wall clock).
fn age_from_birthdate(args: &Value) -> Result<String> {
    let (by, bm, bd) = req_date(args, "birthdate", "age_from_birthdate")?;
    let (oy, om, od) = req_date(args, "on", "age_from_birthdate")?;
    if days_from_civil(oy, om, od) < days_from_civil(by, bm, bd) {
        return Err(anyhow!("age_from_birthdate: 'on' date is before the birthdate"));
    }
    // Whole years: subtract years, then back off one if the birthday hasn't
    // occurred yet in the reference year (month/day comparison).
    let mut age = oy - by;
    if (om, od) < (bm, bd) {
        age -= 1;
    }
    Ok(format!(
        "Born {}, on {} the age is {} year{}",
        fmt_date(by, bm, bd),
        fmt_date(oy, om, od),
        age,
        if age == 1 { "" } else { "s" }
    ))
}

/// `leap_year {year}` -> whether the year is a leap year.
fn leap_year(args: &Value) -> Result<String> {
    let year = args
        .get("year")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("leap_year needs an integer 'year' argument"))?;
    if !(YEAR_MIN..=YEAR_MAX).contains(&year) {
        return Err(anyhow!("leap_year 'year' must be {YEAR_MIN}..={YEAR_MAX}"));
    }
    let leap = is_leap(year);
    Ok(format!(
        "{year} is {}a leap year (February has {} days)",
        if leap { "" } else { "not " },
        if leap { 29 } else { 28 }
    ))
}

/// ISO-8601 week number + ISO week-year of a date. The ISO week-date system:
/// weeks start Monday; week 1 is the week containing the year's first Thursday.
fn iso_week(y: i64, m: i64, d: i64) -> (i64, i64) {
    let serial = days_from_civil(y, m, d);
    // ISO weekday: Monday=1 .. Sunday=7.
    let iso_dow = serial.rem_euclid(7); // 0=Thu..6=Wed (matches WEEKDAYS)
    // Map our Thursday-based index to ISO Mon=1..Sun=7.
    // serial%7: 0=Thu,1=Fri,2=Sat,3=Sun,4=Mon,5=Tue,6=Wed
    let iso_weekday = match iso_dow {
        4 => 1, // Mon
        5 => 2, // Tue
        6 => 3, // Wed
        0 => 4, // Thu
        1 => 5, // Fri
        2 => 6, // Sat
        3 => 7, // Sun
        _ => unreachable!(),
    };
    // The Thursday of this week determines the ISO week-year.
    let thursday_serial = serial - (iso_weekday - 4);
    let (wy, _, _) = civil_from_days(thursday_serial);
    // Week 1 contains Jan 4th (equivalently the first Thursday). Compute the
    // serial of that week-year's Jan 4 and count weeks.
    let jan4_serial = days_from_civil(wy, 1, 4);
    let jan4_iso_dow = jan4_serial.rem_euclid(7);
    let jan4_weekday = match jan4_iso_dow {
        4 => 1,
        5 => 2,
        6 => 3,
        0 => 4,
        1 => 5,
        2 => 6,
        3 => 7,
        _ => unreachable!(),
    };
    let week1_monday = jan4_serial - (jan4_weekday - 1);
    let week = (thursday_serial - week1_monday) / 7 + 1;
    (wy, week)
}

/// `week_of_year {date}` -> ISO week number + week-year.
fn week_of_year(args: &Value) -> Result<String> {
    let (y, m, d) = req_date(args, "date", "week_of_year")?;
    let (wy, week) = iso_week(y, m, d);
    Ok(format!(
        "{} is in ISO week {} of {} ({}-W{:02})",
        fmt_date(y, m, d),
        week,
        wy,
        wy,
        week
    ))
}

/// Parse a Unix epoch-seconds value into (date, h, m, s), UTC, no leap seconds.
fn civil_from_epoch(secs: i64) -> ((i64, i64, i64), i64, i64, i64) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let h = rem / 3_600;
    let mi = (rem % 3_600) / 60;
    let s = rem % 60;
    ((y, mo, d), h, mi, s)
}

/// `unix_to_iso {epoch}` -> 'YYYY-MM-DDTHH:MM:SSZ' (UTC).
fn unix_to_iso(args: &Value) -> Result<String> {
    let epoch = args
        .get("epoch")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("unix_to_iso needs an integer 'epoch' (Unix seconds) argument"))?;
    // Bound to keep the year within range (roughly 0001..9999).
    if !(-62_135_596_800..=253_402_300_799).contains(&epoch) {
        return Err(anyhow!("unix_to_iso 'epoch' is outside the supported year range (0001..9999)"));
    }
    let ((y, mo, d), h, mi, s) = civil_from_epoch(epoch);
    Ok(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, mo, d, h, mi, s
    ))
}

/// Parse an ISO-8601 UTC datetime: 'YYYY-MM-DD' (midnight) or
/// 'YYYY-MM-DDTHH:MM:SS' with an optional trailing 'Z'. Returns epoch seconds.
fn epoch_from_iso(s: &str) -> Result<i64> {
    let s = s.trim();
    let s = s.strip_suffix('Z').unwrap_or(s);
    let (date_part, time_part) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let (y, m, d) = parse_date(date_part)?;
    let (h, mi, sec) = match time_part {
        None => (0i64, 0i64, 0i64),
        Some(t) => {
            let tp: Vec<&str> = t.split(':').collect();
            if tp.len() != 3 {
                return Err(anyhow!("time '{t}' must be HH:MM:SS"));
            }
            let h: i64 = tp[0].parse().map_err(|_| anyhow!("bad hour in '{t}'"))?;
            let mi: i64 = tp[1].parse().map_err(|_| anyhow!("bad minute in '{t}'"))?;
            let sec: i64 = tp[2].parse().map_err(|_| anyhow!("bad second in '{t}'"))?;
            if !(0..=23).contains(&h) || !(0..=59).contains(&mi) || !(0..=60).contains(&sec) {
                return Err(anyhow!("time '{t}' has an out-of-range field"));
            }
            (h, mi, sec.min(59)) // clamp a leap-second 60 to 59 for arithmetic
        }
    };
    Ok(days_from_civil(y, m, d) * 86_400 + h * 3_600 + mi * 60 + sec)
}

/// `iso_to_unix {iso}` -> Unix epoch seconds for an ISO-8601 UTC datetime.
fn iso_to_unix(args: &Value) -> Result<String> {
    let iso = req_str(args, "iso", "iso_to_unix")?;
    let epoch = epoch_from_iso(iso)?;
    Ok(format!("{} -> {} (Unix epoch seconds, UTC)", iso.trim(), epoch))
}

/// `duration_humanize {seconds}` -> 'Xd Yh Zm Ws'. Negative is reported with a
/// leading minus; zero is '0s'.
fn duration_humanize(args: &Value) -> Result<String> {
    let secs = args
        .get("seconds")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("duration_humanize needs an integer 'seconds' argument"))?;
    if secs == 0 {
        return Ok("0s".to_string());
    }
    let neg = secs < 0;
    let mut s = secs.unsigned_abs();
    let days = s / 86_400;
    s %= 86_400;
    let hours = s / 3_600;
    s %= 3_600;
    let mins = s / 60;
    let rem = s % 60;
    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if mins > 0 {
        parts.push(format!("{mins}m"));
    }
    if rem > 0 {
        parts.push(format!("{rem}s"));
    }
    Ok(format!("{}{}", if neg { "-" } else { "" }, parts.join(" ")))
}

/// Parse a fixed UTC offset, given either as integer `*_minutes` or a ±HH:MM
/// string. Returns the offset in minutes, bounded to ±14:00 (the real-world max).
fn parse_offset(args: &Value, min_key: &str, str_key: &str, label: &str) -> Result<i64> {
    if let Some(mins) = args.get(min_key).and_then(Value::as_i64) {
        if !(-840..=840).contains(&mins) {
            return Err(anyhow!("{label} offset minutes must be within ±840 (±14:00)"));
        }
        return Ok(mins);
    }
    if let Some(s) = args.get(str_key).and_then(Value::as_str) {
        return parse_offset_str(s).map_err(|e| anyhow!("{label} {e}"));
    }
    Err(anyhow!(
        "tz_convert needs '{min_key}' (integer minutes) or '{str_key}' (±HH:MM) for the {label} offset"
    ))
}

/// Parse a `±HH:MM` (or `Z`) offset into minutes.
fn parse_offset_str(s: &str) -> Result<i64> {
    let s = s.trim();
    if s == "Z" || s == "+00:00" || s == "-00:00" {
        return Ok(0);
    }
    let (sign, rest) = match s.strip_prefix('+') {
        Some(r) => (1i64, r),
        None => match s.strip_prefix('-') {
            Some(r) => (-1i64, r),
            None => return Err(anyhow!("offset '{s}' must start with + or -")),
        },
    };
    let (hh, mm) = rest
        .split_once(':')
        .ok_or_else(|| anyhow!("offset '{s}' must be ±HH:MM"))?;
    let h: i64 = hh.parse().map_err(|_| anyhow!("offset '{s}' has a bad hour"))?;
    let m: i64 = mm.parse().map_err(|_| anyhow!("offset '{s}' has a bad minute"))?;
    if !(0..=14).contains(&h) || !(0..=59).contains(&m) {
        return Err(anyhow!("offset '{s}' is out of range"));
    }
    let total = sign * (h * 60 + m);
    if !(-840..=840).contains(&total) {
        return Err(anyhow!("offset '{s}' exceeds ±14:00"));
    }
    Ok(total)
}

/// Render an offset in minutes as a ±HH:MM string.
fn fmt_offset(mins: i64) -> String {
    let sign = if mins < 0 { '-' } else { '+' };
    let a = mins.abs();
    format!("{}{:02}:{:02}", sign, a / 60, a % 60)
}

/// `tz_convert {datetime, from_offset_minutes|from_offset, to_offset_minutes|to_offset}`
/// -> the same instant rendered at the target FIXED offset. No tz database, no
/// DST — purely shifts by (to - from) minutes.
fn tz_convert(args: &Value) -> Result<String> {
    let dt = req_str(args, "datetime", "tz_convert")?;
    // Interpret the wall-clock datetime as seconds-from-epoch IN the source zone
    // (i.e. ignoring offset), then re-express at the target offset.
    let local_secs = epoch_from_iso(dt)?;
    let from = parse_offset(args, "from_offset_minutes", "from_offset", "source")?;
    let to = parse_offset(args, "to_offset_minutes", "to_offset", "target")?;
    // local_secs is the source wall clock. Convert to UTC, then to the target.
    let utc = local_secs - from * 60;
    let target = utc + to * 60;
    let ((y, mo, d), h, mi, s) = civil_from_epoch(target);
    Ok(format!(
        "{} {} -> {:04}-{:02}-{:02}T{:02}:{:02}:{:02} {}",
        dt.trim(),
        fmt_offset(from),
        y,
        mo,
        d,
        h,
        mi,
        s,
        fmt_offset(to)
    ))
}

/// Month names for cron explanation (1-based).
const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August",
    "September", "October", "November", "December",
];

/// Day-of-week names for cron (cron: 0 or 7 = Sunday, 1 = Monday).
const CRON_DOW: [&str; 8] = [
    "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday",
];

/// Explain one cron field in plain English. `lo`/`hi` bound the field; `names`
/// optionally maps numeric values to labels (months, weekdays).
fn explain_cron_field(field: &str, lo: i64, hi: i64, unit: &str, names: Option<&[&str]>) -> Result<String> {
    let label = |v: i64| -> String {
        match names {
            Some(ns) if (v as usize) < ns.len() => ns[v as usize].to_string(),
            _ => v.to_string(),
        }
    };
    // Validate each comma-separated term and build a phrase.
    let mut phrases = Vec::new();
    for term in field.split(',') {
        let (base, step) = match term.split_once('/') {
            Some((b, s)) => {
                let st: i64 = s.parse().map_err(|_| anyhow!("cron step '{s}' is not a number"))?;
                if st <= 0 {
                    return Err(anyhow!("cron step must be positive"));
                }
                (b, Some(st))
            }
            None => (term, None),
        };
        match base {
            "*" => {
                if let Some(st) = step {
                    phrases.push(format!("every {st} {unit}s"));
                } else {
                    phrases.push(format!("every {unit}"));
                }
            }
            b if b.contains('-') => {
                let (a, z) = b.split_once('-').unwrap();
                let av: i64 = a.parse().map_err(|_| anyhow!("cron range start '{a}' is not a number"))?;
                let zv: i64 = z.parse().map_err(|_| anyhow!("cron range end '{z}' is not a number"))?;
                if av < lo || zv > hi || av > zv {
                    return Err(anyhow!("cron range {b} is out of {lo}..={hi} for {unit}"));
                }
                match step {
                    Some(st) => phrases.push(format!("every {st} {unit}s from {} to {}", label(av), label(zv))),
                    None => phrases.push(format!("{} through {}", label(av), label(zv))),
                }
            }
            b => {
                let v: i64 = b.parse().map_err(|_| anyhow!("cron value '{b}' is not a number"))?;
                if v < lo || v > hi {
                    return Err(anyhow!("cron value {v} is out of {lo}..={hi} for {unit}"));
                }
                match step {
                    Some(st) => {
                        phrases.push(format!("every {st} {unit}s starting at {}", label(v)))
                    }
                    None => phrases.push(label(v)),
                }
            }
        }
    }
    Ok(phrases.join(", "))
}

/// `cron_explain {expr}` -> plain-English description of a 5-field cron line.
fn cron_explain(args: &Value) -> Result<String> {
    let expr = req_str(args, "expr", "cron_explain")?.trim();
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(anyhow!(
            "cron_explain expects 5 fields (minute hour day-of-month month day-of-week), got {}",
            fields.len()
        ));
    }
    let minute = explain_cron_field(fields[0], 0, 59, "minute", None)?;
    let hour = explain_cron_field(fields[1], 0, 23, "hour", None)?;
    let dom = explain_cron_field(fields[2], 1, 31, "day-of-month", None)?;
    // Month names are 1-based; pad index 0 so label(1)=January.
    let month_names: Vec<&str> = std::iter::once("").chain(MONTHS).collect();
    let month = explain_cron_field(fields[3], 1, 12, "month", Some(&month_names))?;
    let dow = explain_cron_field(fields[4], 0, 7, "day-of-week", Some(&CRON_DOW))?;

    Ok(format!(
        "At minute {minute}, hour {hour}, on day-of-month {dom}, in month {month}, on weekday {dow}."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- core algorithm round-trips ---------------------------------------

    #[test]
    fn civil_day_round_trips_known_anchors() {
        // 1970-01-01 is serial 0; the inverse recovers it.
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-01-01 is 10957 days after the epoch (well-known constant).
        assert_eq!(days_from_civil(2000, 1, 1), 10957);
        assert_eq!(civil_from_days(10957), (2000, 1, 1));
        // A negative serial (before epoch) inverts exactly.
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        // Exhaustive inverse over a long span.
        for z in -400_000..400_000i64 {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
    }

    #[test]
    fn leap_rule_matches_known_years() {
        assert!(is_leap(2000), "2000 divisible by 400");
        assert!(!is_leap(1900), "1900 divisible by 100 but not 400");
        assert!(is_leap(2024));
        assert!(!is_leap(2023));
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2023, 2), 28);
        assert_eq!(days_in_month(2024, 4), 30);
    }

    // ---- date_add ----------------------------------------------------------

    #[test]
    fn date_add_adds_and_subtracts_across_boundaries() {
        // Crossing a month + a leap day: 2024-02-28 + 2 = 2024-03-01? No — 2024
        // is a leap year, so +1 is 2024-02-29, +2 is 2024-03-01.
        let out = date_add(&json!({"date": "2024-02-28", "days": 2})).unwrap();
        assert_eq!(out, "2024-03-01 is 2 days after 2024-02-28");
        // Year boundary forward.
        let out = date_add(&json!({"date": "2023-12-31", "days": 1})).unwrap();
        assert_eq!(out, "2024-01-01 is 1 days after 2023-12-31");
        // Negative count goes backwards.
        let out = date_add(&json!({"date": "2024-01-01", "days": -1})).unwrap();
        assert_eq!(out, "2023-12-31 is 1 days before 2024-01-01");
        // Idempotent.
        assert_eq!(
            date_add(&json!({"date": "2020-06-15", "days": 100})).unwrap(),
            date_add(&json!({"date": "2020-06-15", "days": 100})).unwrap()
        );
    }

    #[test]
    fn date_add_rejects_bad_args() {
        assert!(date_add(&json!({"date": "2024-02-30", "days": 1})).is_err(), "impossible day");
        assert!(date_add(&json!({"date": "2024-13-01", "days": 1})).is_err(), "bad month");
        assert!(date_add(&json!({"date": "not-a-date", "days": 1})).is_err());
        assert!(date_add(&json!({"date": "2024-01-01"})).is_err(), "missing days");
    }

    // ---- weekday_of --------------------------------------------------------

    #[test]
    fn weekday_of_names_known_days() {
        // 1970-01-01 was a Thursday (the epoch anchor).
        assert_eq!(weekday_of(&json!({"date": "1970-01-01"})).unwrap(), "1970-01-01 is a Thursday");
        // 2000-01-01 was a Saturday.
        assert_eq!(weekday_of(&json!({"date": "2000-01-01"})).unwrap(), "2000-01-01 is a Saturday");
        // 2024-02-29 (leap day) was a Thursday.
        assert_eq!(weekday_of(&json!({"date": "2024-02-29"})).unwrap(), "2024-02-29 is a Thursday");
        // The US Declaration date, 1776-07-04, was a Thursday.
        assert_eq!(weekday_name(1776, 7, 4), "Thursday");
    }

    // ---- days_between ------------------------------------------------------

    #[test]
    fn days_between_is_signed_and_exact() {
        let out = days_between(&json!({"from": "2024-01-01", "to": "2024-12-31"})).unwrap();
        assert_eq!(out, "2024-01-01 to 2024-12-31: 365 days"); // 2024 is a leap year
        let out = days_between(&json!({"from": "2023-01-01", "to": "2023-12-31"})).unwrap();
        assert_eq!(out, "2023-01-01 to 2023-12-31: 364 days");
        // Reverse order is negative.
        let out = days_between(&json!({"from": "2024-01-02", "to": "2024-01-01"})).unwrap();
        assert_eq!(out, "2024-01-02 to 2024-01-01: -1 day");
        // Same date is zero.
        let out = days_between(&json!({"from": "2024-01-01", "to": "2024-01-01"})).unwrap();
        assert_eq!(out, "2024-01-01 to 2024-01-01: 0 days");
    }

    // ---- days_until --------------------------------------------------------

    #[test]
    fn days_until_uses_injected_today() {
        let out = days_until(&json!({"today": "2026-06-15", "target": "2026-06-25"})).unwrap();
        assert_eq!(out, "2026-06-25 is in 10 days");
        let out = days_until(&json!({"today": "2026-06-15", "target": "2026-06-15"})).unwrap();
        assert_eq!(out, "2026-06-15 is today");
        let out = days_until(&json!({"today": "2026-06-15", "target": "2026-06-14"})).unwrap();
        assert_eq!(out, "2026-06-14 was 1 day ago");
    }

    // ---- age_from_birthdate ------------------------------------------------

    #[test]
    fn age_handles_birthday_not_yet_reached() {
        // Born 2000-06-15; on 2026-06-15 they turn exactly 26.
        let out = age_from_birthdate(&json!({"birthdate": "2000-06-15", "on": "2026-06-15"})).unwrap();
        assert!(out.ends_with("26 years"), "{out}");
        // One day before the birthday -> still 25.
        let out = age_from_birthdate(&json!({"birthdate": "2000-06-15", "on": "2026-06-14"})).unwrap();
        assert!(out.ends_with("25 years"), "{out}");
        // A leap-day birthday handled on a non-leap year.
        let out = age_from_birthdate(&json!({"birthdate": "2000-02-29", "on": "2023-02-28"})).unwrap();
        assert!(out.ends_with("22 years"), "{out}"); // birthday (2/29) not reached on 2/28
        // 'on' before birthdate is an error, not a negative age.
        assert!(age_from_birthdate(&json!({"birthdate": "2020-01-01", "on": "2019-01-01"})).is_err());
    }

    // ---- leap_year ---------------------------------------------------------

    #[test]
    fn leap_year_skill_reports_correctly() {
        assert_eq!(leap_year(&json!({"year": 2000})).unwrap(), "2000 is a leap year (February has 29 days)");
        assert_eq!(leap_year(&json!({"year": 1900})).unwrap(), "1900 is not a leap year (February has 28 days)");
        assert!(leap_year(&json!({"year": 0})).is_err(), "year out of range");
        assert!(leap_year(&json!({})).is_err(), "missing year");
    }

    // ---- week_of_year ------------------------------------------------------

    #[test]
    fn iso_week_matches_known_cases() {
        // 2026-01-01 is a Thursday -> ISO week 1 of 2026.
        let out = week_of_year(&json!({"date": "2026-01-01"})).unwrap();
        assert!(out.contains("ISO week 1 of 2026"), "{out}");
        assert!(out.contains("2026-W01"), "{out}");
        // 2021-01-01 is a Friday -> belongs to ISO week 53 of 2020.
        let (wy, wk) = iso_week(2021, 1, 1);
        assert_eq!((wy, wk), (2020, 53));
        // 2020-12-31 is a Thursday -> ISO week 53 of 2020.
        assert_eq!(iso_week(2020, 12, 31), (2020, 53));
        // Mid-year: 2024-06-15 is in ISO week 24 of 2024.
        assert_eq!(iso_week(2024, 6, 15), (2024, 24));
    }

    // ---- unix_to_iso / iso_to_unix ----------------------------------------

    #[test]
    fn unix_iso_round_trip_known_values() {
        // 0 epoch is 1970-01-01T00:00:00Z.
        assert_eq!(unix_to_iso(&json!({"epoch": 0})).unwrap(), "1970-01-01T00:00:00Z");
        // A well-known timestamp: 1_700_000_000 = 2023-11-14T22:13:20Z.
        assert_eq!(unix_to_iso(&json!({"epoch": 1_700_000_000i64})).unwrap(), "2023-11-14T22:13:20Z");
        // Inverse recovers the epoch.
        assert_eq!(epoch_from_iso("2023-11-14T22:13:20Z").unwrap(), 1_700_000_000);
        assert_eq!(epoch_from_iso("1970-01-01T00:00:00Z").unwrap(), 0);
        // A bare date is midnight UTC.
        assert_eq!(epoch_from_iso("2000-01-01").unwrap(), 946_684_800);
        // Negative epoch (before 1970) works both ways.
        assert_eq!(unix_to_iso(&json!({"epoch": -1})).unwrap(), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn iso_to_unix_skill_and_errors() {
        let out = iso_to_unix(&json!({"iso": "2023-11-14T22:13:20Z"})).unwrap();
        assert!(out.contains("1700000000"), "{out}");
        assert!(iso_to_unix(&json!({"iso": "2024-13-01"})).is_err(), "bad month");
        assert!(iso_to_unix(&json!({"iso": "2024-01-01T25:00:00"})).is_err(), "bad hour");
        assert!(iso_to_unix(&json!({})).is_err(), "missing iso");
    }

    // ---- duration_humanize -------------------------------------------------

    #[test]
    fn duration_humanize_known_values() {
        assert_eq!(duration_humanize(&json!({"seconds": 0})).unwrap(), "0s");
        assert_eq!(duration_humanize(&json!({"seconds": 59})).unwrap(), "59s");
        assert_eq!(duration_humanize(&json!({"seconds": 60})).unwrap(), "1m");
        assert_eq!(duration_humanize(&json!({"seconds": 3661})).unwrap(), "1h 1m 1s");
        // 1 day, 1 hour exactly (no minutes/seconds shown).
        assert_eq!(duration_humanize(&json!({"seconds": 90_000})).unwrap(), "1d 1h");
        // Negative.
        assert_eq!(duration_humanize(&json!({"seconds": -3661})).unwrap(), "-1h 1m 1s");
        assert!(duration_humanize(&json!({})).is_err());
    }

    // ---- tz_convert --------------------------------------------------------

    #[test]
    fn tz_convert_shifts_by_fixed_offset() {
        // New York (-05:00) noon to Tokyo (+09:00) is 14 hours ahead -> 02:00 next day.
        let out = tz_convert(&json!({
            "datetime": "2024-01-01T12:00:00",
            "from_offset": "-05:00",
            "to_offset": "+09:00"
        }))
        .unwrap();
        assert!(out.contains("2024-01-02T02:00:00 +09:00"), "{out}");
        assert!(out.contains("-05:00"), "{out}");
        // Same offset is a no-op on the wall clock.
        let out = tz_convert(&json!({
            "datetime": "2024-06-15T08:30:00",
            "from_offset_minutes": 0,
            "to_offset_minutes": 0
        }))
        .unwrap();
        assert!(out.contains("2024-06-15T08:30:00 +00:00"), "{out}");
        // Minutes form: +05:30 (India) from UTC.
        let out = tz_convert(&json!({
            "datetime": "2024-06-15T00:00:00",
            "from_offset_minutes": 0,
            "to_offset_minutes": 330
        }))
        .unwrap();
        assert!(out.contains("2024-06-15T05:30:00 +05:30"), "{out}");
    }

    #[test]
    fn tz_convert_rejects_bad_offsets() {
        assert!(tz_convert(&json!({"datetime": "2024-01-01T00:00:00", "from_offset": "+15:00", "to_offset": "Z"})).is_err());
        assert!(tz_convert(&json!({"datetime": "2024-01-01T00:00:00"})).is_err(), "missing offsets");
    }

    // ---- cron_explain ------------------------------------------------------

    #[test]
    fn cron_explain_known_expressions() {
        // Every day at 00:00.
        let out = cron_explain(&json!({"expr": "0 0 * * *"})).unwrap();
        assert_eq!(
            out,
            "At minute 0, hour 0, on day-of-month every day-of-month, in month every month, on weekday every day-of-week."
        );
        // Every 15 minutes.
        let out = cron_explain(&json!({"expr": "*/15 * * * *"})).unwrap();
        assert!(out.contains("At minute every 15 minutes"), "{out}");
        // Weekday names + month names resolve.
        let out = cron_explain(&json!({"expr": "30 9 * 1 1"})).unwrap();
        assert!(out.contains("in month January"), "{out}");
        assert!(out.contains("on weekday Monday"), "{out}");
        // Range + list.
        let out = cron_explain(&json!({"expr": "0 9-17 * * 1-5"})).unwrap();
        assert!(out.contains("hour 9 through 17"), "{out}");
        assert!(out.contains("Monday through Friday"), "{out}");
        // Bare-value step form `<start>/<step>` (e.g. 0/15): the step must be
        // honored, not silently dropped.
        let out = cron_explain(&json!({"expr": "0/15 * * * *"})).unwrap();
        assert!(out.contains("every 15 minutes starting at 0"), "{out}");
    }

    #[test]
    fn cron_explain_validates_shape_and_ranges() {
        assert!(cron_explain(&json!({"expr": "* * *"})).is_err(), "wrong field count");
        assert!(cron_explain(&json!({"expr": "60 * * * *"})).is_err(), "minute 60 out of range");
        assert!(cron_explain(&json!({"expr": "* 24 * * *"})).is_err(), "hour 24 out of range");
        assert!(cron_explain(&json!({"expr": "* * * 13 *"})).is_err(), "month 13 out of range");
        assert!(cron_explain(&json!({"expr": "* * * * 8"})).is_err(), "dow 8 out of range");
        assert!(cron_explain(&json!({})).is_err(), "missing expr");
    }

    // ---- catalog -----------------------------------------------------------

    #[test]
    fn catalog_is_all_pure_and_well_formed() {
        let s = skills();
        assert!(s.len() >= 8 && s.len() <= 12, "8-12 datetime skills, got {}", s.len());
        for d in &s {
            assert_eq!(d.category, Category::Datetime);
            assert!(!d.consequential, "{} must be pure", d.name);
            assert!(!d.source_gated, "{} needs no external source", d.name);
            assert!(super::super::is_snake_case(d.name), "{} snake_case", d.name);
            assert!(!d.description.is_empty());
            assert!(!d.cues.is_empty(), "{} needs cues", d.name);
        }
        // No duplicate names within the category.
        let mut names: Vec<&str> = s.iter().map(|d| d.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "no duplicate names");
    }
}
