//! Deterministic currency detection from statement text.

use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize)]
pub struct Currency {
    pub symbol: String,
    pub code: String,
    pub name: String,
    pub confidence: f64,
    pub detected_by: String,
}

struct Def {
    symbol: &'static str,
    code: &'static str,
    name: &'static str,
    /// Extra word/code aliases that strongly imply this currency.
    aliases: &'static [&'static str],
}

const DEFS: &[Def] = &[
    Def { symbol: "₦", code: "NGN", name: "Nigerian Naira", aliases: &["ngn", "naira"] },
    Def { symbol: "$", code: "USD", name: "US Dollar", aliases: &["usd", "us$", "dollar"] },
    Def { symbol: "£", code: "GBP", name: "British Pound", aliases: &["gbp", "pound", "sterling"] },
    Def { symbol: "€", code: "EUR", name: "Euro", aliases: &["eur", "euro"] },
    Def { symbol: "¥", code: "JPY", name: "Japanese Yen", aliases: &["jpy", "yen"] },
    Def { symbol: "₹", code: "INR", name: "Indian Rupee", aliases: &["inr", "rupee"] },
    Def { symbol: "₵", code: "GHS", name: "Ghanaian Cedi", aliases: &["ghs", "cedi"] },
    Def { symbol: "R", code: "ZAR", name: "South African Rand", aliases: &["zar", "rand"] },
    Def { symbol: "KSh", code: "KES", name: "Kenyan Shilling", aliases: &["kes", "ksh", "shilling"] },
    Def { symbol: "₿", code: "BTC", name: "Bitcoin", aliases: &["btc", "bitcoin", "sats"] },
    Def { symbol: "C$", code: "CAD", name: "Canadian Dollar", aliases: &["cad"] },
    Def { symbol: "A$", code: "AUD", name: "Australian Dollar", aliases: &["aud"] },
    Def { symbol: "Fr", code: "CHF", name: "Swiss Franc", aliases: &["chf", "franc"] },
    Def { symbol: "₩", code: "KRW", name: "South Korean Won", aliases: &["krw", "won"] },
    Def { symbol: "د.إ", code: "AED", name: "UAE Dirham", aliases: &["aed", "dirham"] },
];

/// Scan the full text once and pick the highest-scoring currency.
pub fn detect(text: &str) -> Currency {
    let lower = text.to_lowercase();
    let mut scores: HashMap<usize, (u64, &'static str)> = HashMap::new();

    for (i, def) in DEFS.iter().enumerate() {
        let mut score = 0u64;
        let mut how = "symbol";

        // Unicode currency symbols (₦, $, £, €, …) are unambiguous and weighted
        // heavily. Plain-letter symbols like "R"/"Fr"/"KSh" are skipped here.
        if def.symbol.chars().any(|c| !c.is_ascii_alphanumeric()) {
            let n = text.matches(def.symbol).count() as u64;
            score += n * 12;
            if n > 0 {
                how = "symbol";
            }
        }
        // Code/alias occurrences, but only as whole words (so "kes" inside
        // another word can't falsely trigger Kenyan Shilling).
        for alias in def.aliases {
            let n = count_word(&lower, alias) as u64;
            if n > 0 {
                score += n * 8;
                if how != "symbol" {
                    how = "code/keyword";
                }
            }
        }
        if score > 0 {
            scores.insert(i, (score, how));
        }
    }

    if let Some((&idx, &(score, how))) = scores.iter().max_by_key(|(_, (s, _))| *s) {
        let total: u64 = scores.values().map(|(s, _)| *s).sum();
        let def = &DEFS[idx];
        return Currency {
            symbol: def.symbol.to_string(),
            code: def.code.to_string(),
            name: def.name.to_string(),
            confidence: if total > 0 {
                (score as f64 / total as f64 * 100.0).round() / 100.0
            } else {
                0.0
            },
            detected_by: how.to_string(),
        };
    }

    Currency {
        symbol: "?".into(),
        code: "UNKNOWN".into(),
        name: "Undetermined".into(),
        confidence: 0.0,
        detected_by: "none".into(),
    }
}

/// Count whole-word (boundary-delimited) occurrences of `word` in `text`.
fn count_word(text: &str, word: &str) -> usize {
    let bytes = text.as_bytes();
    let wlen = word.len();
    let mut count = 0;
    let mut start = 0;
    while let Some(pos) = text[start..].find(word) {
        let i = start + pos;
        let before = if i == 0 { None } else { bytes.get(i - 1).copied() };
        let after = bytes.get(i + wlen).copied();
        let boundary = |b: Option<u8>| b.map_or(true, |c| !c.is_ascii_alphanumeric());
        if boundary(before) && boundary(after) {
            count += 1;
        }
        start = i + wlen;
    }
    count
}
