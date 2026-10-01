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
    /// Optional amount band, e.g. "20k-45k", "over 10k", "around 30k".
    pub amount_min: Option<f64>,
    pub amount_max: Option<f64>,
    /// Optional anchor: "… after I paid ndo 30k" / "… before I sent X".
    pub anchor: Option<Anchor>,
    /// Time-of-day windows (hour ranges, inclusive) from "at night", "in the morning", etc.
    /// Empty = any time. Transactions without a clock time are never excluded by this.
    pub hours: Vec<(u32, u32)>,
    pub human: String,
    /// True if we detected a date, direction, amount band or anchor.
    pub smart: bool,
}

/// Another transaction the query is relative to ("a few hours AFTER I paid Ndo 30k").
#[derive(Debug, Clone)]
pub struct Anchor {
    /// Distinctive words identifying the anchor transaction (e.g. "ndo").
    pub keyword: String,
    /// Amount of the anchor transaction, if stated (e.g. 30000 for "30k").
    pub amount: Option<f64>,
    /// true = look AFTER the anchor, false = BEFORE it (ignored when `around`).
    pub after: bool,
    /// true = either side of the anchor ("around the time I paid ndo").
    pub around: bool,
    /// Size of the window in hours, inferred from phrasing:
    /// "few hours / shortly / right after" = 6, "that night / next morning" = 18,
    /// "within 12 hours" = 12, otherwise 12.
    pub window_hours: i64,
    /// Pin the anchor to a specific day ("…after I paid ndo 30k ON 25 MARCH").
    pub on_date: Option<NaiveDate>,
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
    // relative / anchor phrasing
    "transfer", "transfers", "transferred", "made", "make", "few", "couple", "hour", "hours",
    "minute", "minutes", "mins", "right", "shortly", "later", "earlier", "after", "before",
    "around", "about", "approximately", "roughly", "under", "below", "above", "more", "less",
    "than", "least", "most", "up", "called", "named", "someone", "somebody", "person", "guy",
    "girl", "her", "him", "his", "she", "he", "them", "they", "it", "name", "fee", "fees",
    "yoruba", "igbo", "hausa", "naira", "ngn",
    // question words
    "what", "which", "who", "whom", "whose", "where", "when", "why", "whats", "did",
    // vague / time-of-day phrasing ("probably in the night or early morning at the hotel room")
    "time", "moment", "then", "or", "so", "like", "probably", "maybe", "perhaps", "think",
    "thought", "remember", "cant", "can't", "dont", "don't", "night", "nights", "morning",
    "evening", "afternoon", "overnight", "midnight", "early", "late", "within", "room",
    "there", "thats", "that's", "im", "i'm", "using", "narrow", "down", "also", "came",
    "come", "left", "leave", "stayed", "stay", "same", "following", "next",
];

const MONTHS: &[(&str, u32)] = &[
    ("january", 1), ("february", 2), ("march", 3), ("april", 4), ("may", 5), ("june", 6),
    ("july", 7), ("august", 8), ("september", 9), ("october", 10), ("november", 11), ("december", 12),
    ("jan", 1), ("feb", 2), ("mar", 3), ("apr", 4), ("jun", 6), ("jul", 7), ("aug", 8),
    ("sep", 9), ("sept", 9), ("oct", 10), ("nov", 11), ("dec", 12),
];

static LAST_N: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:last|past)\s+(\d+)\s+(day|week|month|year)s?").unwrap());

static ANCHOR_SPLIT: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(after|before|around|near)\b").unwrap());
static WINDOW_N: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(\d{1,3})\s*(?:hours?|hrs?|h)\b").unwrap());
static WINDOW_LONG: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"overnight|that night|the night|at night|in the night|next morning|morning after|the morning|early morning|by morning|following morning|next day|the day after|same day").unwrap()
});
static WINDOW_SHORT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(few|couple|shortly|right|immediately|just|minutes?|mins?|straight)\b").unwrap());

static DATE_DM: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b(\d{1,2})(?:st|nd|rd|th)?\s+(jan|january|feb|february|mar|march|apr|april|may|jun|june|jul|july|aug|august|sep|sept|september|oct|october|nov|november|dec|december)(?:\s+(\d{4}))?\b").unwrap()
});
static DATE_MD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b(jan|january|feb|february|mar|march|apr|april|may|jun|june|jul|july|aug|august|sep|sept|september|oct|october|nov|november|dec|december)\s+(\d{1,2})(?:st|nd|rd|th)?(?:,?\s+(\d{4}))?\b").unwrap()
});
static DATE_NUM: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(\d{1,2})[/-](\d{1,2})(?:[/-](\d{2,4}))?\b").unwrap());

fn month_num(name: &str) -> Option<u32> {
    MONTHS.iter().find(|(n, _)| *n == name).map(|(_, m)| *m)
}

/// A specific calendar day: "25 march", "march 25th", "25/3", "25-03-2026".
/// Year defaults to the current one when omitted.
fn detect_specific_date(s: &str, today: NaiveDate) -> Option<NaiveDate> {
    let year_or = |y: Option<regex::Match>| -> i32 {
        y.and_then(|m| m.as_str().parse::<i32>().ok())
            .map(|v| if v < 100 { 2000 + v } else { v })
            .unwrap_or(today.year())
    };
    if let Some(c) = DATE_DM.captures(s) {
        let d: u32 = c[1].parse().ok()?;
        let m = month_num(&c[2])?;
        return NaiveDate::from_ymd_opt(year_or(c.get(3)), m, d);
    }
    if let Some(c) = DATE_MD.captures(s) {
        let m = month_num(&c[1])?;
        let d: u32 = c[2].parse().ok()?;
        return NaiveDate::from_ymd_opt(year_or(c.get(3)), m, d);
    }
    if let Some(c) = DATE_NUM.captures(s) {
        // Day-first (25/3). Only accept when it can't be an amount or a time.
        let d: u32 = c[1].parse().ok()?;
        let m: u32 = c[2].parse().ok()?;
        if (1..=31).contains(&d) && (1..=12).contains(&m) {
            return NaiveDate::from_ymd_opt(year_or(c.get(3)), m, d);
        }
    }
    None
}

/// Time-of-day windows mentioned in the query (union of all mentioned).
fn detect_time_of_day(s: &str) -> Vec<(u32, u32)> {
    let mut v: Vec<(u32, u32)> = Vec::new();
    let has = |w: &str| contains_word(s, w);
    if has("night") || has("nights") || has("overnight") || has("midnight") || has("tonight") {
        v.push((20, 23));
        v.push((0, 5));
    }
    if s.contains("early morning") || has("dawn") {
        v.push((3, 8));
    }
    if has("morning") {
        v.push((5, 11));
    }
    if has("afternoon") || has("noon") {
        v.push((12, 16));
    }
    if has("evening") {
        v.push((17, 20));
    }
    v
}

/// How wide the anchor window is, from how the user phrased it.
fn detect_window_hours(l: &str) -> i64 {
    if let Some(c) = WINDOW_N.captures(l) {
        if let Ok(n) = c[1].parse::<i64>() {
            return n.clamp(1, 168);
        }
    }
    if WINDOW_LONG.is_match(l) {
        return 18;
    }
    if WINDOW_SHORT.is_match(l) {
        return 6;
    }
    12
}
static AMT_RANGE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(\d+(?:[.,]\d+)*)\s*(k|m)?\s*(?:-|–|to|and)\s*(\d+(?:[.,]\d+)*)\s*(k|m)?\b").unwrap()
});
static AMT_OVER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:over|above|more than|at least|from)\s+(\d+(?:[.,]\d+)*)\s*(k|m)?\b").unwrap()
});
static AMT_UNDER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:under|below|less than|at most|up to)\s+(\d+(?:[.,]\d+)*)\s*(k|m)?\b").unwrap()
});
static AMT_AROUND: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:around|about|approximately|roughly|~)\s*(\d+(?:[.,]\d+)*)\s*(k|m)?\b").unwrap()
});
static AMT_ANY: Lazy<Regex> = Lazy::new(|| Regex::new(r"(\d+(?:[.,]\d+)*)\s*(k|m)\b|(\d{1,3}(?:,\d{3})+|\d{4,})").unwrap());

pub fn parse(query: &str, today: NaiveDate) -> QueryFilter {
    let raw = query.trim();
    if raw.is_empty() {
        return QueryFilter::default();
    }
    let lower = raw.to_lowercase();

    // 0) Anchor split: "<target> after|before <anchor clause>".
    //    Only treated as an anchor if the clause names something (a keyword or amount).
    let mut target = lower.clone();
    let mut anchor: Option<Anchor> = None;
    if let Some(m) = ANCHOR_SPLIT.find_iter(&lower).last() {
        let word = m.as_str();
        let clause = lower[m.end()..].trim();
        let a_amount = first_amount(clause);
        let a_keyword = extract_keyword(clause);
        let is_around = word == "around" || word == "near";
        // "around 30k" is an amount band, not an anchor: around/near need a named thing.
        let ok = if is_around { !a_keyword.is_empty() } else { !a_keyword.is_empty() || a_amount.is_some() };
        if ok {
            anchor = Some(Anchor {
                keyword: a_keyword,
                amount: a_amount,
                after: word == "after",
                around: is_around,
                window_hours: detect_window_hours(&lower),
                on_date: detect_specific_date(clause, today),
            });
            target = lower[..m.start()].trim().to_string();
        }
    }

    // Time of day ("in the night", "early morning") applies to the target.
    let hours = detect_time_of_day(&target);

    // 1) Direction (from the target; fall back to the whole query)
    let direction = detect_direction(&target).or_else(|| detect_direction(&lower));

    // 2) Date range (from the target)
    let (date_from, date_to, date_label) = detect_range(&target, today);

    // 3) Amount band (from the target; if the user put it after the anchor,
    //    e.g. "...after I paid ndo 30k, 20k-45k", look at the whole query).
    let (mut amount_min, mut amount_max) = detect_amount_band(&target);
    if amount_min.is_none() && amount_max.is_none() && anchor.is_some() {
        let (lo, hi) = detect_amount_band(&lower);
        // Only accept an explicit two-sided range here, so the anchor's own
        // "30k" can never be mistaken for a band.
        if lo.is_some() && hi.is_some() {
            amount_min = lo;
            amount_max = hi;
        }
    }

    // 4) Keyword (strip fillers, time words, verbs, months, numbers)
    let keyword = extract_keyword(&target);

    let smart = date_from.is_some()
        || direction.is_some()
        || amount_min.is_some()
        || amount_max.is_some()
        || anchor.is_some()
        || !hours.is_empty();

    let human = build_human(&keyword, direction, &date_label, amount_min, amount_max, anchor.as_ref(), &hours);

    QueryFilter { keyword, date_from, date_to, direction, amount_min, amount_max, anchor, hours, human, smart }
}

fn money_val(num: &str, suffix: Option<&str>) -> Option<f64> {
    let n: f64 = num.replace(',', "").parse().ok()?;
    Some(match suffix {
        Some("k") => n * 1_000.0,
        Some("m") => n * 1_000_000.0,
        _ => n,
    })
}

/// First amount in a clause: "30k", "1.5m", "20,000", or a bare number of 4+ digits.
fn first_amount(s: &str) -> Option<f64> {
    let c = AMT_ANY.captures(s)?;
    if let Some(n) = c.get(1) {
        money_val(n.as_str(), c.get(2).map(|m| m.as_str()))
    } else {
        money_val(c.get(3)?.as_str(), None)
    }
}

/// "20k-45k" / "between 20k and 45k" / "over 10k" / "under 5k" / "around 30k".
fn detect_amount_band(s: &str) -> (Option<f64>, Option<f64>) {
    if let Some(c) = AMT_RANGE.captures(s) {
        let lo_suf = c.get(2).map(|m| m.as_str()).or_else(|| c.get(4).map(|m| m.as_str()));
        let lo = money_val(&c[1], lo_suf);
        let hi = money_val(&c[3], c.get(4).map(|m| m.as_str()));
        if let (Some(a), Some(b)) = (lo, hi) {
            return (Some(a.min(b)), Some(a.max(b)));
        }
    }
    let mut lo = AMT_OVER.captures(s).and_then(|c| money_val(&c[1], c.get(2).map(|m| m.as_str())));
    let mut hi = AMT_UNDER.captures(s).and_then(|c| money_val(&c[1], c.get(2).map(|m| m.as_str())));
    if lo.is_none() && hi.is_none() {
        if let Some(c) = AMT_AROUND.captures(s) {
            if let Some(v) = money_val(&c[1], c.get(2).map(|m| m.as_str())) {
                lo = Some(v * 0.85);
                hi = Some(v * 1.15);
            }
        }
    }
    (lo, hi)
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
    // A specific day beats everything: "on 25 march", "march 25th", "25/3".
    if let Some(d) = detect_specific_date(l, today) {
        return (Some(d), Some(d), d.format("%-d %B %Y").to_string());
    }
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
    // Number-like tokens: 30, 30k, 1.5m, 20,000, 20k-45k, 45k.
    let numberish = |w: &str| {
        w.chars().any(|c| c.is_ascii_digit())
            && w.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | '-' | 'k' | 'm'))
    };
    let tokens: Vec<&str> = cleaned
        .split_whitespace()
        .filter(|w| {
            let w = *w;
            !STOPWORDS.contains(&w) && !month_names.contains(&w) && !numberish(w)
        })
        .collect();
    tokens.join(" ").trim().to_string()
}

fn fmt_amt(v: f64) -> String {
    let whole = v.round() as i64;
    let s = whole.abs().to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if whole < 0 { format!("-{out}") } else { out }
}

fn build_human(
    keyword: &str,
    dir: Option<Direction>,
    date_label: &str,
    amount_min: Option<f64>,
    amount_max: Option<f64>,
    anchor: Option<&Anchor>,
    hours: &[(u32, u32)],
) -> String {
    let mut parts: Vec<String> = Vec::new();
    match dir {
        Some(Direction::Debit) => parts.push("money out".into()),
        Some(Direction::Credit) => parts.push("money in".into()),
        _ => {}
    }
    if !keyword.is_empty() {
        parts.push(format!("“{keyword}”"));
    }
    if !hours.is_empty() {
        let lbl: Vec<String> = hours.iter().map(|(a, b)| format!("{a:02}:00-{b:02}:59")).collect();
        parts.push(format!("· {}", lbl.join(" or ")));
    }
    match (amount_min, amount_max) {
        (Some(a), Some(b)) => parts.push(format!("· {} to {}", fmt_amt(a), fmt_amt(b))),
        (Some(a), None) => parts.push(format!("· over {}", fmt_amt(a))),
        (None, Some(b)) => parts.push(format!("· under {}", fmt_amt(b))),
        _ => {}
    }
    if !date_label.is_empty() {
        parts.push(format!("· {date_label}"));
    }
    if let Some(a) = anchor {
        let mut what = String::new();
        if !a.keyword.is_empty() {
            what.push_str(&format!("“{}”", a.keyword));
        }
        if let Some(v) = a.amount {
            if !what.is_empty() {
                what.push(' ');
            }
            what.push_str(&format!("({})", fmt_amt(v)));
        }
        let rel = if a.around { "either side of" } else if a.after { "after" } else { "before" };
        if let Some(d) = a.on_date {
            what.push_str(&format!(" on {}", d.format("%-d %b")));
        }
        parts.push(format!("· within {}h {} the {} payment", a.window_hours, rel, what));
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
