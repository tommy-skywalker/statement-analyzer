//! Deterministic parsing helpers for money amounts and dates.
//! No AI, no locale guessing beyond well-defined heuristics.

use chrono::NaiveDate;
use once_cell::sync::Lazy;
use regex::Regex;

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
    Regex::new(r"[-+]?\(?\s*[0-9][0-9.,\s]*[0-9]\)?").unwrap()
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

    // Trailing/leading DR / CR markers (common in African & UK statements).
    if lower.contains("dr") && !lower.contains("credit") {
        negative_hint = true;
    }

    // Normalise unicode minus and remove everything that is not part of a number.
    s = s.replace(['\u{2212}', '−'], "-");

    // Detect explicit sign before stripping.
    if s.trim_start().starts_with('-') {
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
    let mut out = Vec::new();
    for m in AMOUNT_TOKEN.find_iter(line) {
        let tok = m.as_str();
        let digits: String = tok.chars().filter(|c| c.is_ascii_digit()).collect();
        let has_sep = tok.contains('.') || tok.contains(',');
        // A bare integer must be >=5 digits to count (filters day/month/year
        // fragments like 05, 01, 2024). Anything with a decimal/thousands
        // separator is always treated as a real amount.
        if !has_sep && digits.len() < 5 {
            continue;
        }
        if digits.len() == 4 && !has_sep {
            continue; // year
        }
        if let Some(money) = parse_amount(tok) {
            out.push((money, m.range()));
        }
    }
    out
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
    for fmt in DATE_FORMATS {
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            if reasonable(d) {
                return Some(d);
            }
        }
    }
    None
}

static DATE_IN_TEXT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?ix)
        \b(
            \d{4}[-/]\d{1,2}[-/]\d{1,2}
          | \d{1,2}[-/.]\d{1,2}[-/.]\d{2,4}
          | \d{1,2}[\s-][A-Za-z]{3,9}[\s-]\d{2,4}
          | [A-Za-z]{3,9}\s+\d{1,2},?\s+\d{2,4}
        )\b",
    )
    .unwrap()
});

/// Find and parse the first date appearing in a free-text line.
pub fn find_date_in_line(line: &str) -> Option<NaiveDate> {
    for cap in DATE_IN_TEXT.captures_iter(line) {
        if let Some(m) = cap.get(1) {
            if let Some(d) = parse_date(m.as_str()) {
                return Some(d);
            }
        }
    }
    None
}

fn reasonable(d: NaiveDate) -> bool {
    use chrono::Datelike;
    d.year() >= 1990 && d.year() <= 2100
}
