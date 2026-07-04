//! Deterministic natural-language query parsing ("smart search").
//! Turns phrases like "how much did I spend on chicken last month" into a
//! structured filter: keyword + optional date range + optional direction.
//! No AI — pure rules, so it stays fast and predictable.

use crate::model::Direction;
use chrono::{Datelike, Duration, Months, NaiveDate};
use once_cell::sync::Lazy;
use regex::Regex;

#[derive(Debug, Default)]
pub struct QueryFilter {
    pub keyword: String,
    pub date_from: Option<NaiveDate>,
    pub date_to: Option<NaiveDate>,
    pub direction: Option<Direction>,
    pub human: String,
    /// True if we detected a date or direction (i.e. richer than a plain keyword).
    pub smart: bool,
}

const DEBIT_VERBS: &[&str] = &[
    "spend", "spent", "spending", "paid", "pay", "sent", "send", "sending", "bought", "buy",
    "buying", "purchase", "purchased", "withdraw", "withdrew", "withdrawal", "cost",
];
const CREDIT_VERBS: &[&str] =
    &["received", "receive", "receiving", "earned", "earn", "credited", "inflow", "deposited"];

// Words removed when isolating the keyword/entity.
const STOPWORDS: &[&str] = &[
    "how", "much", "many", "did", "does", "do", "i", "you", "my", "me", "mine", "we", "our",
    "the", "a", "an", "on", "at", "in", "of", "for", "to", "and", "that", "with", "is", "was",
    "were", "been", "show", "tell", "know", "knowing", "wanna", "want", "see", "find", "get",
    "getting", "total", "all", "sum", "amount", "transactions", "transaction", "txns", "txn",
    "everything", "anything", "something", "every", "any", "whole", "entire",
    "account", "number", "acct", "money", "cash", "spent", "spend", "spending", "paid", "pay",
    "sent", "send", "sending", "bought", "buy", "buying", "purchase", "purchased", "withdraw",
    "withdrew", "withdrawal", "received", "receive", "receiving", "earned", "earn", "credited",
    "deposited", "cost", "last", "this", "past", "next", "ago", "between", "over", "during",
    "since", "day", "days", "week", "weeks", "month", "months", "year", "years", "today",
    "yesterday", "recent", "recently", "from", "into", "out",
];

const MONTHS: &[(&str, u32)] = &[
    ("january", 1), ("february", 2), ("march", 3), ("april", 4), ("may", 5), ("june", 6),
    ("july", 7), ("august", 8), ("september", 9), ("october", 10), ("november", 11), ("december", 12),
    ("jan", 1), ("feb", 2), ("mar", 3), ("apr", 4), ("jun", 6), ("jul", 7), ("aug", 8),
    ("sep", 9), ("sept", 9), ("oct", 10), ("nov", 11), ("dec", 12),
];

static LAST_N: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:last|past)\s+(\d+)\s+(day|week|month|year)s?").unwrap());

pub fn parse(query: &str, today: NaiveDate) -> QueryFilter {
    let raw = query.trim();
    if raw.is_empty() {
        return QueryFilter::default();
    }
    let lower = raw.to_lowercase();

    // 1) Direction
    let direction = detect_direction(&lower);

    // 2) Date range
    let (date_from, date_to, date_label) = detect_range(&lower, today);

    // 3) Keyword (strip fillers, time words, direction verbs, months, numbers)
    let keyword = extract_keyword(&lower);

    let smart = date_from.is_some() || direction.is_some();

    let human = build_human(&keyword, direction, &date_label);

    QueryFilter { keyword, date_from, date_to, direction, human, smart }
}

fn detect_direction(l: &str) -> Option<Direction> {
    let has = |words: &[&str]| words.iter().any(|w| contains_word(l, w));
    if l.contains("money out") || l.contains("paid out") {
        return Some(Direction::Debit);
    }
    if l.contains("money in") || l.contains("paid in") {
        return Some(Direction::Credit);
    }
    if has(DEBIT_VERBS) {
        Some(Direction::Debit)
    } else if has(CREDIT_VERBS) {
        Some(Direction::Credit)
    } else {
        None
    }
}

fn detect_range(l: &str, today: NaiveDate) -> (Option<NaiveDate>, Option<NaiveDate>, String) {
    // "last/past N days|weeks|months|years"
    if let Some(cap) = LAST_N.captures(l) {
        let n: i64 = cap[1].parse().unwrap_or(1);
        let unit = &cap[2];
        let from = match unit {
            "day" => today - Duration::days(n),
            "week" => today - Duration::weeks(n),
            "month" => sub_months(today, n as u32),
            _ => sub_months(today, (n * 12) as u32),
        };
        return (Some(from), Some(today), format!("last {n} {unit}{}", plural(n)));
    }

    if l.contains("today") {
        return (Some(today), Some(today), "today".into());
    }
    if l.contains("yesterday") {
        let y = today - Duration::days(1);
        return (Some(y), Some(y), "yesterday".into());
    }
    if l.contains("this week") {
        let start = today - Duration::days(today.weekday().num_days_from_monday() as i64);
        return (Some(start), Some(today), "this week".into());
    }
    if l.contains("last week") {
        let this_start = today - Duration::days(today.weekday().num_days_from_monday() as i64);
        let start = this_start - Duration::days(7);
        let end = this_start - Duration::days(1);
        return (Some(start), Some(end), "last week".into());
    }
    if l.contains("this month") {
        let start = first_of_month(today);
        return (Some(start), Some(today), month_label(today));
    }
    if l.contains("last month") {
        let end = first_of_month(today) - Duration::days(1);
        let start = first_of_month(end);
        return (Some(start), Some(end), month_label(start));
    }
    if l.contains("this year") {
        let start = NaiveDate::from_ymd_opt(today.year(), 1, 1).unwrap();
        return (Some(start), Some(today), format!("{}", today.year()));
    }
    if l.contains("last year") {
        let y = today.year() - 1;
        let start = NaiveDate::from_ymd_opt(y, 1, 1).unwrap();
        let end = NaiveDate::from_ymd_opt(y, 12, 31).unwrap();
        return (Some(start), Some(end), format!("{y}"));
    }

    // bare month name, optionally with a year
    if let Some((mname, mnum)) = MONTHS.iter().find(|(n, _)| contains_word(l, n)) {
        // avoid treating "may" as a month when used as the verb "may"
        if *mname == "may" && !l.contains("in may") && !l.contains("may 20") {
            // skip ambiguous "may"
        } else {
            let year = extract_year(l).unwrap_or_else(|| {
                if *mnum <= today.month() { today.year() } else { today.year() - 1 }
            });
            let start = NaiveDate::from_ymd_opt(year, *mnum, 1).unwrap();
            let end = first_of_month(add_months(start, 1)) - Duration::days(1);
            return (Some(start), Some(end), month_label(start));
        }
    }

    (None, None, String::new())
}

fn extract_keyword(l: &str) -> String {
    let cleaned: String = l
        .chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '\'' || c == '-' { c } else { ' ' })
        .collect();
    let month_names: Vec<&str> = MONTHS.iter().map(|(n, _)| *n).collect();
    let tokens: Vec<&str> = cleaned
        .split_whitespace()
        .filter(|w| {
            let w = *w;
            !STOPWORDS.contains(&w)
                && !month_names.contains(&w)
                && !w.chars().all(|c| c.is_ascii_digit())
        })
        .collect();
    tokens.join(" ").trim().to_string()
}

fn build_human(keyword: &str, dir: Option<Direction>, date_label: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    match dir {
        Some(Direction::Debit) => parts.push("money out".into()),
        Some(Direction::Credit) => parts.push("money in".into()),
        _ => {}
    }
    if !keyword.is_empty() {
        parts.push(format!("“{keyword}”"));
    }
    if !date_label.is_empty() {
        parts.push(format!("· {date_label}"));
    }
    if parts.is_empty() {
        "all transactions".into()
    } else {
        parts.join(" ")
    }
}

// ---- helpers ----

fn contains_word(haystack: &str, word: &str) -> bool {
    if word.contains(' ') {
        return haystack.contains(word);
    }
    haystack
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w == word)
}

fn extract_year(l: &str) -> Option<i32> {
    static YEAR: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(20\d{2})\b").unwrap());
    YEAR.captures(l).and_then(|c| c[1].parse().ok())
}

fn first_of_month(d: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap()
}
fn add_months(d: NaiveDate, n: u32) -> NaiveDate {
    d.checked_add_months(Months::new(n)).unwrap_or(d)
}
fn sub_months(d: NaiveDate, n: u32) -> NaiveDate {
    d.checked_sub_months(Months::new(n)).unwrap_or(d)
}
fn month_label(d: NaiveDate) -> String {
    d.format("%B %Y").to_string()
}
fn plural(n: i64) -> &'static str {
    if n == 1 { "" } else { "s" }
}
