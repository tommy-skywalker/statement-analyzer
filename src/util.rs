//! Deterministic parsing helpers for money amounts and dates.
//! No AI, no locale guessing beyond well-defined heuristics.

use chrono::{NaiveDate, NaiveTime};
use once_cell::sync::Lazy;
use regex::Regex;

static TIME_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(\d{1,2}):(\d{2})(?::(\d{2}))?\b").unwrap());

/// Find the first clock time (HH:MM or HH:MM:SS) in a line.
pub fn find_time_in_line(line: &str) -> Option<NaiveTime> {
    let c = TIME_RE.captures(line)?;
    let h: u32 = c.get(1)?.as_str().parse().ok()?;
    let m: u32 = c.get(2)?.as_str().parse().ok()?;
    let s: u32 = c.get(3).and_then(|x| x.as_str().parse().ok()).unwrap_or(0);
    if h < 24 && m < 60 && s < 60 {
        NaiveTime::from_hms_opt(h, m, s)
    } else {
        None
    }
}

/// A parsed monetary value. `value` is signed (negative = outflow hint),
/// `magnitude` is always the absolute amount.
#[derive(Debug, Clone, Copy)]
pub struct Money {
    pub value: f64,
    pub negative_hint: bool,
}

impl Money {
    pub fn magnitude(&self) -> f64 {
        self.value.abs()
    }
}

static AMOUNT_TOKEN: Lazy<Regex> = Lazy::new(|| {
    // Matches a number that looks like money inside a larger string.
    // No whitespace inside the token, so adjacent amounts stay separate.
    Regex::new(r"[-+]?\(?[0-9][0-9.,]*[0-9]\)?").unwrap()
});

/// Parse a single cell/string that is expected to be (mostly) a number.
/// Handles currency symbols, thousands separators, parentheses-negatives,
/// trailing/leading DR/CR markers and unicode minus.
pub fn parse_amount(raw: &str) -> Option<Money> {
    let mut s = raw.trim().to_string();
    if s.is_empty() {
        return None;
    }

    let lower = s.to_lowercase();
    let mut negative_hint = false;

    // Accounting parentheses -> negative.
    if s.starts_with('(') && s.trim_end_matches(['c', 'r', 'd', ' ', '\t']).ends_with(')')
        || (s.contains('(') && s.contains(')'))
    {
        negative_hint = true;
    }

    // Trailing/leading DR / OD markers as whole words ("12.50 DR", "DR 12.50").
    if lower.split(|c: char| !c.is_ascii_alphabetic()).any(|w| w == "dr" || w == "od") {
        negative_hint = true;
    }

    // Normalise unicode minus and remove everything that is not part of a number.
    s = s.replace(['\u{2212}', '−', '–'], "-");

    // Explicit sign: before the first digit ("-12.50", "£-12.50", "- £12.50")
    // or trailing ("12.50-").
    let first_digit = s.find(|c: char| c.is_ascii_digit()).unwrap_or(s.len());
    if s[..first_digit].contains('-') || s.trim_end().ends_with('-') {
        negative_hint = true;
    }

    // Keep only digits, separators and signs.
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == ',' || *c == '-')
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() || !cleaned.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }

    let normalised = normalise_separators(&cleaned);
    let parsed: f64 = normalised.parse().ok()?;

    // Reject values that are not realistic money: non-finite, or absurdly large
    // (long account / reference / phone numbers get mis-read as huge amounts).
    if !parsed.is_finite() || parsed.abs() >= 1e13 {
        return None;
    }
    // A bare integer (no decimal separator) longer than 12 digits is almost
    // certainly an identifier, not an amount.
    let int_digits = normalised.split('.').next().unwrap_or("").trim_start_matches('-').len();
    if !normalised.contains('.') && int_digits > 12 {
        return None;
    }

    let value = if negative_hint { -parsed.abs() } else { parsed };
    Some(Money {
        value,
        negative_hint,
    })
}

/// Resolve thousands vs decimal separators deterministically.
fn normalise_separators(s: &str) -> String {
    let has_comma = s.contains(',');
    let has_dot = s.contains('.');

    match (has_comma, has_dot) {
        (true, true) => {
            // The right-most separator is the decimal point.
            let last_comma = s.rfind(',').unwrap();
            let last_dot = s.rfind('.').unwrap();
            if last_dot > last_comma {
                // 1,234.56  -> remove commas
                s.replace(',', "")
            } else {
                // 1.234,56  -> remove dots, comma becomes decimal
                s.replace('.', "").replace(',', ".")
            }
        }
        (true, false) => {
            // Only commas. Comma is decimal iff exactly one and 1-2 trailing digits.
            let parts: Vec<&str> = s.split(',').collect();
            if parts.len() == 2 && (parts[1].len() == 2 || parts[1].len() == 1) {
                s.replace(',', ".")
            } else {
                s.replace(',', "")
            }
        }
        (false, true) => {
            // Only dots. Dot is thousands sep iff multiple dots or all groups are 3 digits.
            let parts: Vec<&str> = s.split('.').collect();
            if parts.len() > 2 || (parts.len() == 2 && parts[1].len() == 3) {
                s.replace('.', "")
            } else {
                s.to_string()
            }
        }
        (false, false) => s.to_string(),
    }
}

/// Find all money-looking tokens inside a free-text line, left to right.
pub fn find_amounts_in_line(line: &str) -> Vec<(Money, std::ops::Range<usize>)> {
    let mut out: Vec<(Money, std::ops::Range<usize>, bool)> = Vec::new();
    for m in AMOUNT_TOKEN.find_iter(line) {
        let tok = m.as_str();
        let digits: String = tok.chars().filter(|c| c.is_ascii_digit()).collect();
        let has_sep = tok.contains('.') || tok.contains(',');
        // Treat a trailing ".dd" (1-2 digits) as a real decimal amount.
        let has_decimal = tok
            .rsplit_once('.')
            .map(|(_, frac)| (1..=2).contains(&frac.chars().filter(|c| c.is_ascii_digit()).count()))
            .unwrap_or(false);
        // A bare integer must be >=5 digits to count (filters day/month/year
        // fragments like 05, 01, 2024). Anything with a decimal/thousands
        // separator is always treated as a real amount.
        if !has_sep && digits.len() < 5 {
            continue;
        }
        if digits.len() == 4 && !has_sep {
            continue; // year
        }
        // A bare integer with 10+ digits is an account / reference / phone
        // number, not a transaction amount — skip it.
        if !has_sep && digits.len() >= 10 {
            continue;
        }
        if let Some(money) = parse_amount(tok) {
            out.push((money, m.range(), has_decimal));
        }
    }
    // If any token is a proper decimal amount (e.g. 1,234.56), keep only those —
    // this drops bare reference/id numbers that happen to sit on the line.
    if out.iter().any(|(_, _, dec)| *dec) {
        out.retain(|(_, _, dec)| *dec);
    }
    out.into_iter().map(|(m, r, _)| (m, r)).collect()
}

static DATE_FORMATS: &[&str] = &[
    "%Y-%m-%d",
    "%Y/%m/%d",
    "%d/%m/%Y",
    "%d-%m-%Y",
    "%d.%m.%Y",
    "%m/%d/%Y",
    "%m-%d-%Y",
    "%d/%m/%y",
    "%d-%m-%y",
    "%d %b %Y",
    "%d-%b-%Y",
    "%d %B %Y",
    "%b %d, %Y",
    "%B %d, %Y",
    "%d-%b-%y",
    "%d %b %y",
    "%Y%m%d",
];

/// Try to parse a cell that should be a date.
pub fn parse_date(raw: &str) -> Option<NaiveDate> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // Drop a trailing clock time ("05/10/2026 12:34", "2026-10-05T12:34:56Z").
    let s = TRAILING_TIME.replace(s, "");
    let s = s.trim().trim_end_matches('T');
    for fmt in DATE_FORMATS {
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            if reasonable(d) {
                return Some(d);
            }
        }
    }
    None
}

static TRAILING_TIME: Lazy<Regex> = Lazy::new(|| Regex::new(r"[T\s]+\d{1,2}:\d{2}.*$").unwrap());

static YEARLESS_DATE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?ix)
        \b(?:
            (?P<d1>\d{1,2})(?:st|nd|rd|th)?\s+(?P<m1>jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*
          | (?P<m2>jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\s+(?P<d2>\d{1,2})(?:st|nd|rd|th)?
        )\b",
    )
    .unwrap()
});

/// Find a date written without a year ("5 Oct", "Oct 5") and give it `year`.
/// Returns the date and the byte range it occupies.
pub fn find_yearless_date(line: &str, year: i32) -> Option<(NaiveDate, std::ops::Range<usize>)> {
    let c = YEARLESS_DATE.captures(line)?;
    let (d, m) = match (c.name("d1"), c.name("m1"), c.name("d2"), c.name("m2")) {
        (Some(d), Some(m), _, _) => (d, m),
        (_, _, Some(d), Some(m)) => (d, m),
        _ => return None,
    };
    let month = match m.as_str().to_lowercase().as_str() {
        "jan" => 1, "feb" => 2, "mar" => 3, "apr" => 4, "may" => 5, "jun" => 6,
        "jul" => 7, "aug" => 8, "sep" => 9, "oct" => 10, "nov" => 11, _ => 12,
    };
    // "SEP 2,100.00" or "1.5 Sep" is a month name next to a figure, not a date.
    let whole = c.get(0)?;
    let glued = |ch: Option<char>| matches!(ch, Some('.') | Some(',') | Some('/') | Some('-') | Some(':'));
    let after = &line[whole.end()..];
    if glued(after.chars().next()) && after.chars().nth(1).map_or(false, |x| x.is_ascii_digit()) {
        return None;
    }
    let before = &line[..whole.start()];
    if glued(before.chars().next_back()) {
        return None;
    }
    let day: u32 = d.as_str().parse().ok()?;
    NaiveDate::from_ymd_opt(year, month, day).map(|dt| (dt, c.get(0).map_or(0..0, |x| x.range())))
}

static DATE_IN_TEXT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?ix)
        \b(
            \d{4}[-/]\d{1,2}[-/]\d{1,2}(?:\b|T)
          | \d{1,2}[-/.]\d{1,2}[-/.]\d{2,4}\b
          | \d{1,2}[\s-][A-Za-z]{3,9}[\s-]\d{2,4}\b
          | [A-Za-z]{3,9}\s+\d{1,2},?\s+\d{2,4}\b
        )",
    )
    .unwrap()
});

/// Find and parse the first date appearing in a free-text line.
pub fn find_date_in_line(line: &str) -> Option<NaiveDate> {
    find_date_span(line).map(|(d, _)| d)
}

/// Like `find_date_in_line`, also returning the byte range the date occupies.
pub fn find_date_span(line: &str) -> Option<(NaiveDate, std::ops::Range<usize>)> {
    for cap in DATE_IN_TEXT.captures_iter(line) {
        if let Some(m) = cap.get(1) {
            if let Some(d) = parse_date(m.as_str()) {
                return Some((d, m.range()));
            }
        }
    }
    None
}

fn reasonable(d: NaiveDate) -> bool {
    use chrono::Datelike;
    d.year() >= 1990 && d.year() <= 2100
}
