//! The deterministic analysis engine: builds transactions from extracted
//! blocks, classifies debit/credit, and computes per-side statistics.

use crate::extract::{self, Block};
use crate::model::*;
use crate::util::{self, Money};
use chrono::{Datelike, NaiveDate};
use once_cell::sync::Lazy;
use regex::Regex;
use std::time::Instant;

const MATCHED_CAP: usize = 2000;

// Statement-table row reconstruction (for wrapped PDF text like OPay):
// each transaction starts with a date + time; amounts are `debit credit balance`
// with `--` for empties and proper `.dd` decimals.
static ROW_START: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\d{1,2}\s+[A-Za-z]{3,9}\s+\d{4}\s+\d{1,2}:\d{2}:\d{2}").unwrap());
static AMT_OR_DASH: Lazy<Regex> = Lazy::new(|| Regex::new(r"--|\d[\d,]*\.\d{2}").unwrap());
static ROW_PREFIX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\s*\d{1,2}\s+[A-Za-z]{3,9}\s+\d{4}\s+\d{1,2}:\d{2}:\d{2}\s+\d{1,2}\s+[A-Za-z]{3,9}\s+\d{4}\s*").unwrap()
});

/// A parsed statement: the expensive extraction result, cached so keyword
/// changes can re-filter instantly without re-parsing the file.
pub struct ParsedDoc {
    pub filename: String,
    pub kind: String,
    pub size_bytes: usize,
    pub parts: Vec<String>,
    pub currency: crate::currency::Currency,
    pub txns: Vec<Transaction>,
    pub warnings: Vec<String>,
}

/// Extract + build transactions (the slow part). Cache the result.
pub fn parse(filename: &str, bytes: &[u8]) -> Result<ParsedDoc, String> {
    let extracted = extract::extract(filename, bytes).map_err(|e| format!("Extraction failed: {e}"))?;
    // Currency detection only needs a sample — avoid materialising a second
    // full copy of a large document just to scan for symbols.
    let currency = crate::currency::detect(&extracted.raw_text_sample(256 * 1024));

    let mut warnings = extracted.warnings.clone();
    let mut multi_account = false;
    let mut txns: Vec<Transaction> = Vec::new();
    for block in &extracted.blocks {
        match block {
            Block::Table { source, rows } => table_to_txns(source, rows, &mut txns),
            Block::Text { source, lines } => {
                if lines.iter().filter(|l| l.to_lowercase().contains("opening balance")).count() >= 2 {
                    multi_account = true;
                }
                text_to_txns(source, lines, &mut txns);
            }
        }
    }
    if txns.is_empty() {
        warnings.push(
            "No transactions could be parsed from this file. Check that it contains tabular or line-based statement data.".into(),
        );
    }
    if multi_account {
        warnings.push(
            "This statement contains more than one account; analysing only the first (primary) account.".into(),
        );
    }
    Ok(ParsedDoc {
        filename: filename.to_string(),
        kind: extracted.kind,
        size_bytes: bytes.len(),
        parts: dedup(extracted.parts),
        currency,
        txns,
        warnings,
    })
}

pub fn run(filename: &str, bytes: &[u8], query: &str) -> AnalysisResult {
    match parse(filename, bytes) {
        Ok(doc) => analyze_parsed(&doc, query),
        Err(msg) => error_result(filename, query, bytes.len(), msg),
    }
}

/// Parse + analyze, also returning the parsed doc (for caching) on success.
pub fn parse_and_analyze(filename: &str, bytes: &[u8], query: &str) -> (AnalysisResult, Option<ParsedDoc>) {
    match parse(filename, bytes) {
        Ok(doc) => {
            let r = analyze_parsed(&doc, query);
            (r, Some(doc))
        }
        Err(msg) => (error_result(filename, query, bytes.len(), msg), None),
    }
}

/// Filter + compute stats over an already-parsed document (the fast part).
pub fn analyze_parsed(doc: &ParsedDoc, query: &str) -> AnalysisResult {
    let started = Instant::now();
    let size = doc.size_bytes;
    let currency = doc.currency.clone();
    let txns = &doc.txns;
    let mut warnings = doc.warnings.clone();

    // Smart natural-language query: keyword + optional date range + direction.
    let today = chrono::Utc::now().date_naive();
    let f = crate::nlquery::parse(query, today);
    let kw = f.keyword.to_lowercase();
    let no_filter = kw.is_empty()
        && f.date_from.is_none()
        && f.direction.is_none()
        && f.amount_min.is_none()
        && f.amount_max.is_none()
        && f.anchor.is_none();
    if no_filter {
        warnings.push("No search term provided, analysing ALL transactions.".into());
    }

    // Resolve the anchor ("… after I paid ndo 30k") to one concrete transaction.
    let anchor_txn: Option<&Transaction> = f.anchor.as_ref().and_then(|a| {
        let akw = a.keyword.to_lowercase();
        let found = txns.iter().find(|t| {
            t.date.is_some()
                && a.on_date.map_or(true, |d| t.date == Some(d))
                && tokens_any_match(&t.description, &t.raw, &akw)
                && a.amount.map_or(true, |v| (t.amount - v).abs() <= (v * 0.02).max(50.0))
        });
        if found.is_none() {
            warnings.push(format!(
                "Couldn't find the anchor transaction ({}{}). Showing results without it.",
                if a.keyword.is_empty() { "any".to_string() } else { format!("“{}”", a.keyword) },
                a.amount.map(|v| format!(", {}", fmt_money(&currency.symbol, v))).unwrap_or_default()
            ));
        }
        found
    });
    if let (Some(a), Some(t)) = (f.anchor.as_ref(), anchor_txn) {
        let rel = if a.around { "either side of" } else if a.after { "after" } else { "before" };
        warnings.push(format!(
            "Anchored {} “{}” on {}{} ({}). Showing transactions within {} hours.",
            rel,
            truncate(&t.description, 60),
            t.date.map(|d| d.to_string()).unwrap_or_default(),
            t.time.map(|tm| format!(" {}", tm.format("%H:%M"))).unwrap_or_default(),
            fmt_money(&currency.symbol, t.amount),
            a.window_hours
        ));
    }

    let in_window = |t: &Transaction| -> bool {
        let (a, anc) = match (f.anchor.as_ref(), anchor_txn) {
            (Some(a), Some(anc)) => (a, anc),
            _ => return true,
        };
        if std::ptr::eq(t, anc) {
            return false; // never return the anchor itself
        }
        let window = chrono::Duration::hours(a.window_hours);
        match (anc.date, anc.time, t.date, t.time) {
            (Some(ad), Some(at), Some(td), Some(tt)) => {
                // Real datetimes: crosses midnight naturally ("early morning" after a night payment).
                let diff = td.and_time(tt).signed_duration_since(ad.and_time(at));
                if a.around {
                    diff != chrono::Duration::zero() && diff.abs() <= window
                } else if a.after {
                    diff > chrono::Duration::zero() && diff <= window
                } else {
                    diff < chrono::Duration::zero() && -diff <= window
                }
            }
            (Some(ad), _, Some(td), _) => {
                // No clock times: use statement order, allowing the adjacent day.
                let day_gap = (td - ad).num_days();
                let same_src = t.source == anc.source;
                if a.around {
                    day_gap.abs() <= 1 && same_src
                } else if a.after {
                    same_src && (day_gap == 1 || (day_gap == 0 && t.line_no > anc.line_no))
                } else {
                    same_src && (day_gap == -1 || (day_gap == 0 && t.line_no < anc.line_no))
                }
            }
            _ => false,
        }
    };

    let matched: Vec<&Transaction> = txns
        .iter()
        .filter(|t| {
            let kw_ok = kw.is_empty()
                || t.description.to_lowercase().contains(&kw)
                || t.raw.to_lowercase().contains(&kw);
            let date_ok = match (f.date_from, f.date_to) {
                (Some(a), Some(b)) => t.date.map_or(false, |d| d >= a && d <= b),
                _ => true,
            };
            let dir_ok = f.direction.map_or(true, |dir| t.direction == dir);
            let amt_ok = f.amount_min.map_or(true, |lo| t.amount >= lo) && f.amount_max.map_or(true, |hi| t.amount <= hi);
            // Time of day ("in the night"): only applied when the transaction has a clock time.
            let tod_ok = f.hours.is_empty()
                || t.time.map_or(true, |tm| {
                    let h = chrono::Timelike::hour(&tm);
                    f.hours.iter().any(|(a, b)| h >= *a && h <= *b)
                });
            kw_ok && date_ok && dir_ok && amt_ok && tod_ok && in_window(t)
        })
        .collect();
    // "last 3 months" counts back from today; say so when the statement is older.
    if let (true, Some(from), Some(to)) = (matched.is_empty(), f.date_from, f.date_to) {
        let dates: Vec<NaiveDate> = txns.iter().filter_map(|t| t.date).collect();
        if let (Some(lo), Some(hi)) = (dates.iter().min(), dates.iter().max()) {
            if from > *hi || to < *lo {
                warnings.push(format!(
                    "This statement covers {lo} to {hi}, so nothing falls in the dates you asked for ({from} to {to}). Try naming the months, for example \"{}\".",
                    hi.format("%B %Y")
                ));
            }
        }
    }

    let interpreted = Interpreted {
        keyword: f.keyword.clone(),
        direction: f.direction.map(|d| match d {
            Direction::Debit => "debit".to_string(),
            Direction::Credit => "credit".to_string(),
            Direction::Unknown => "unknown".to_string(),
        }),
        date_from: f.date_from.map(|d| d.to_string()),
        date_to: f.date_to.map(|d| d.to_string()),
        human: f.human.clone(),
        smart: f.smart,
    };

    let debits: Vec<&&Transaction> = matched.iter().filter(|t| t.direction == Direction::Debit).collect();
    let credits: Vec<&&Transaction> = matched.iter().filter(|t| t.direction == Direction::Credit).collect();

    let sym = currency.symbol.clone();
    let debit_stats = side_stats("Debit", &debits, &sym);
    let credit_stats = side_stats("Credit", &credits, &sym);

    let net = credit_stats.total - debit_stats.total;
    let (ofirst, olast) = overall_range(&matched);
    let overall_duration = match (ofirst, olast) {
        (Some(a), Some(b)) => Some(duration_between(a, b)),
        _ => None,
    };

    // Build matched list for the UI.
    let mut matched_out: Vec<MatchedTxn> = Vec::new();
    for t in matched.iter().take(MATCHED_CAP) {
        matched_out.push(MatchedTxn {
            date: t.date.map(|d| d.to_string()),
            time: t.time.map(|tm| tm.format("%H:%M").to_string()),
            description: truncate(&t.description, 200),
            amount: round2(t.amount),
            direction: t.direction,
            source: t.source.clone(),
        });
    }
    let matched_truncated = matched.len() > MATCHED_CAP;

    let elapsed = started.elapsed();
    let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
    let mb = size as f64 / (1024.0 * 1024.0);
    let throughput = if elapsed.as_secs_f64() > 0.0 { mb / elapsed.as_secs_f64() } else { 0.0 };

    AnalysisResult {
        ok: true,
        query: query.to_string(),
        interpreted,
        cache_id: None,
        file: FileMeta {
            name: doc.filename.clone(),
            kind: doc.kind.clone(),
            size_bytes: size,
            size_human: human_size(size),
            parts: doc.parts.clone(),
        },
        currency,
        summary: Summary {
            total_transactions_scanned: txns.len(),
            matched_transactions: matched.len(),
            net_amount: round2(net),
            net_formatted: fmt_money(&sym, net),
            overall_first_date: ofirst.map(|d| d.to_string()),
            overall_last_date: olast.map(|d| d.to_string()),
            overall_duration,
        },
        debit: debit_stats,
        credit: credit_stats,
        matched: matched_out,
        matched_truncated,
        warnings,
        elapsed_ms: round2(elapsed_ms),
        throughput_mb_s: round2(throughput),
    }
}

// --------------------------- Tabular path ---------------------------

#[derive(Default, Clone)]
struct ColMap {
    date: Option<usize>,
    description: Option<usize>,
    debit: Option<usize>,
    credit: Option<usize>,
    amount: Option<usize>,
    balance: Option<usize>,
    type_ind: Option<usize>,
}

fn header_score(cells: &[String]) -> u32 {
    // A header row never contains real data: a parseable date means this is a data row.
    if cells.iter().any(|c| util::parse_date(c).is_some()) {
        return 0;
    }
    let mut s = 0;
    for c in cells {
        let l = c.to_lowercase();
        let l = l.trim();
        for kw in [
            "date", "description", "narration", "narrative", "details", "particular", "transaction",
            "debit", "withdrawal", "credit", "deposit", "amount", "balance", "value",
            "reference", "type", "dr", "cr", "memo", "payee", "paid out", "paid in",
            "money out", "money in", "counter party", "category", "currency", "notes",
        ] {
            if l == kw || (l.contains(kw) && l.len() <= kw.len() + 12) {
                s += 1;
                break;
            }
        }
    }
    s
}

/// How good a column title is as the human-readable description (0 = not one).
fn description_rank(l: &str) -> u8 {
    if l.starts_with("account") || l.ends_with(" id") || l == "id" {
        0
    } else if l.contains("description") || l.contains("narrati") {
        9
    } else if l.contains("details") || l.contains("particular") {
        8
    } else if l.contains("memo") {
        7
    } else if l.contains("counter party") || l.contains("counterparty") || l.contains("payee") || l.contains("merchant") || l == "name" {
        6
    } else if l.contains("reference") {
        4
    } else if l.contains("transaction") {
        3
    } else if l.contains("notes") {
        2
    } else {
        0
    }
}

fn map_header(cells: &[String]) -> ColMap {
    let mut m = ColMap::default();
    let mut best_desc = 0u8;
    for (i, c) in cells.iter().enumerate() {
        let l = c.to_lowercase();
        let l = l.trim();
        if l.is_empty() {
            continue;
        }
        let set = |slot: &mut Option<usize>, idx: usize| {
            if slot.is_none() {
                *slot = Some(idx);
            }
        };
        let both = (l.contains("debit") && l.contains("credit")) || l.contains("dr/cr") || l.contains("cr/dr");
        if l.contains("balance") {
            set(&mut m.balance, i);
        } else if both || l == "type" || l.ends_with(" type") || l.contains("indicator") {
            set(&mut m.type_ind, i);
        } else if l.contains("withdraw") || l == "dr" || l == "out" || l.contains("debit") || l.contains("money out") || l.contains("paid out") {
            set(&mut m.debit, i);
        } else if l.contains("deposit") || l == "cr" || l == "in" || l.contains("credit") || l.contains("money in") || l.contains("paid in") {
            set(&mut m.credit, i);
        } else if l.contains("date") || l.contains("posted") {
            set(&mut m.date, i);
        } else if l.contains("amount") || l.contains("value") {
            set(&mut m.amount, i);
        } else {
            let rank = description_rank(l);
            if rank > best_desc {
                best_desc = rank;
                m.description = Some(i);
            }
        }
    }
    m
}

fn table_to_txns(source: &str, rows: &[Vec<String>], out: &mut Vec<Transaction>) {
    if rows.is_empty() {
        return;
    }

    // 1. Locate header.
    let mut header_idx: Option<usize> = None;
    let mut best = 1u32;
    for (i, row) in rows.iter().take(30).enumerate() {
        let s = header_score(row);
        if s > best {
            best = s;
            header_idx = Some(i);
        }
    }

    let (map, start) = match header_idx {
        Some(i) => (map_header(&rows[i]), i + 1),
        None => (infer_columns(rows), 0),
    };

    let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);

    // One signed Amount column: if any value is negative, the sign is the
    // direction. If none is, the sign says nothing and we lean on the type
    // column and the running balance instead.
    let single_amount = map.debit.is_none() && map.credit.is_none() && map.amount.is_some();
    let signed = single_amount
        && rows.iter().skip(start).any(|r| {
            cell(r, map.amount).and_then(util::parse_amount).map_or(false, |a| a.value < 0.0 || a.negative_hint)
        });

    let first_new = out.len();
    for (ri, row) in rows.iter().enumerate().skip(start) {
        if row.iter().all(|c| c.trim().is_empty()) {
            continue;
        }
        if let Some(t) = row_to_txn(source, ri, row, &map, ncols, signed) {
            out.push(t);
        }
    }
    if single_amount && !signed {
        fix_directions_by_balance(&mut out[first_new..], &std::collections::HashMap::new());
    }
}

/// Heuristically infer columns when there is no recognisable header.
fn infer_columns(rows: &[Vec<String>]) -> ColMap {
    let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let sample: Vec<&Vec<String>> = rows.iter().take(200).collect();
    let mut date_frac = vec![0.0f64; ncols];
    let mut num_frac = vec![0.0f64; ncols];
    let mut text_len = vec![0.0f64; ncols];
    let mut counts = vec![0usize; ncols];

    for row in &sample {
        for (c, cell) in row.iter().enumerate() {
            if cell.trim().is_empty() {
                continue;
            }
            counts[c] += 1;
            if util::parse_date(cell).is_some() {
                date_frac[c] += 1.0;
            }
            if util::parse_amount(cell).is_some() {
                num_frac[c] += 1.0;
            }
            text_len[c] += cell.chars().count() as f64;
        }
    }
    for c in 0..ncols {
        let n = counts[c].max(1) as f64;
        date_frac[c] /= n;
        num_frac[c] /= n;
        text_len[c] /= n;
    }

    let mut m = ColMap::default();
    // Date column: best date fraction.
    m.date = (0..ncols).filter(|&c| date_frac[c] > 0.5).max_by(|&a, &b| date_frac[a].total_cmp(&date_frac[b]));
    // Description: most text, low numeric.
    m.description = (0..ncols)
        .filter(|&c| Some(c) != m.date && num_frac[c] < 0.5)
        .max_by(|&a, &b| text_len[a].total_cmp(&text_len[b]));
    // Amount columns: numeric, not the date/description.
    let mut numeric_cols: Vec<usize> = (0..ncols)
        .filter(|&c| Some(c) != m.date && num_frac[c] > 0.5)
        .collect();
    numeric_cols.sort_by(|&a, &b| num_frac[b].total_cmp(&num_frac[a]));
    match numeric_cols.len() {
        0 => {}
        1 => m.amount = Some(numeric_cols[0]),
        _ => {
            // Assume last numeric column is balance; the one before it is amount.
            let mut by_pos = numeric_cols.clone();
            by_pos.sort();
            m.balance = by_pos.last().copied();
            m.amount = by_pos.get(by_pos.len().saturating_sub(2)).copied();
        }
    }
    m
}

fn cell<'a>(row: &'a [String], idx: Option<usize>) -> Option<&'a str> {
    idx.and_then(|i| row.get(i)).map(|s| s.as_str()).filter(|s| !s.trim().is_empty())
}

fn row_to_txn(source: &str, line_no: usize, row: &[String], m: &ColMap, _ncols: usize, signed: bool) -> Option<Transaction> {
    let joined = row.join(" ");
    let date = cell(row, m.date)
        .and_then(util::parse_date)
        .or_else(|| util::find_date_in_line(&joined));

    let description = cell(row, m.description)
        .map(|s| s.trim().trim_start_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            // Fall back to the longest non-numeric cell.
            row.iter()
                .filter(|c| util::parse_amount(c).is_none() && util::parse_date(c).is_none())
                .max_by_key(|c| c.len())
                .cloned()
                .unwrap_or_default()
        });

    let balance = cell(row, m.balance).and_then(util::parse_amount).map(|x| x.value);

    // Determine amount + direction.
    let (amount, direction) = if m.debit.is_some() || m.credit.is_some() {
        let d = cell(row, m.debit).and_then(util::parse_amount).map(|x| x.magnitude()).filter(|x| *x > 0.0);
        let c = cell(row, m.credit).and_then(util::parse_amount).map(|x| x.magnitude()).filter(|x| *x > 0.0);
        match (d, c) {
            (Some(d), _) => (d, Direction::Debit),
            (None, Some(c)) => (c, Direction::Credit),
            (None, None) => return None,
        }
    } else if let Some(a) = cell(row, m.amount).and_then(util::parse_amount) {
        let dir = if a.value < 0.0 || a.negative_hint {
            Direction::Debit
        } else if signed {
            Direction::Credit
        } else {
            direction_from_type(cell(row, m.type_ind)).unwrap_or_else(|| classify_text(&joined, &a))
        };
        (a.magnitude(), dir)
    } else {
        // Last resort: scan the whole row text.
        let amts = util::find_amounts_in_line(&joined);
        let money = amts.first()?.0;
        let dir = classify_text(&joined, &money);
        (money.magnitude(), dir)
    };

    if amount == 0.0 {
        return None;
    }

    Some(Transaction {
        date,
        time: cell(row, m.date).and_then(util::find_time_in_line).or_else(|| util::find_time_in_line(&joined)),
        description: clean(&description),
        amount,
        direction,
        balance,
        raw: clean(&joined),
        source: source.to_string(),
        line_no,
    })
}

fn direction_from_type(t: Option<&str>) -> Option<Direction> {
    let t = t?.to_lowercase();
    let t = t.trim();
    if matches!(t, "c" | "cr" | "fpi" | "bgc" | "dep" | "in") || t.contains("credit") || t.contains("deposit") {
        Some(Direction::Credit)
    } else if matches!(t, "d" | "dr" | "dd" | "d/d" | "so" | "s/o" | "deb" | "fpo" | "cpt" | "chq" | "pos" | "atm" | "out")
        || t.contains("debit")
        || t.contains("withdraw")
        || t.contains("card payment")
    {
        Some(Direction::Debit)
    } else {
        None
    }
}

/// Settle money-in vs money-out from the running balance. Works on one
/// account's transactions in file order (oldest or newest first), including
/// statements that print the balance only on some rows. `known_before[i]` is
/// a balance known to apply just before `txns[i]` (e.g. "brought forward").
/// Returns true if the balance trail was consistent enough to be applied.
fn fix_directions_by_balance(txns: &mut [Transaction], known_before: &std::collections::HashMap<usize, f64>) -> bool {
    let cents = |x: f64| (x * 100.0).round() as i64;
    let n = txns.len();
    if n == 0 {
        return false;
    }

    // Walk in chronological order; `order` maps step -> index in `txns`.
    let solve = |order: &[usize], use_known: bool| -> (usize, usize, Vec<(usize, Direction)>) {
        let mut assigned: Vec<(usize, Direction)> = Vec::new();
        let (mut solved, mut checked) = (0usize, 0usize);
        let mut last_bal: Option<i64> = None;
        let mut group: Vec<usize> = Vec::new();
        for &i in order {
            if use_known {
                if let Some(b) = known_before.get(&i) {
                    last_bal = Some(cents(*b));
                    group.clear();
                }
            }
            group.push(i);
            let Some(b) = txns[i].balance else { continue };
            let b = cents(b);
            if let Some(p) = last_bal {
                checked += group.len();
                let delta = b - p;
                let k = group.len();
                if k <= 16 {
                    // Find credit/debit signs whose sum equals the balance change,
                    // preferring the combination closest to the current guess.
                    let mut best: Option<(u32, u32)> = None; // (disagreements, mask)
                    for mask in 0u32..(1u32 << k) {
                        let mut sum = 0i64;
                        let mut dis = 0u32;
                        for (j, &ti) in group.iter().enumerate() {
                            let credit = mask & (1 << j) != 0;
                            let a = cents(txns[ti].amount);
                            sum += if credit { a } else { -a };
                            if credit != (txns[ti].direction == Direction::Credit) {
                                dis += 1;
                            }
                        }
                        if sum == delta && best.map_or(true, |(d, _)| dis < d) {
                            best = Some((dis, mask));
                        }
                    }
                    if let Some((_, mask)) = best {
                        solved += k;
                        for (j, &ti) in group.iter().enumerate() {
                            let dir = if mask & (1 << j) != 0 { Direction::Credit } else { Direction::Debit };
                            assigned.push((ti, dir));
                        }
                    }
                }
            }
            last_bal = Some(b);
            group.clear();
        }
        (solved, checked, assigned)
    };

    let fwd_order: Vec<usize> = (0..n).collect();
    let rev_order: Vec<usize> = (0..n).rev().collect();
    let fwd = solve(&fwd_order, true);
    let rev = solve(&rev_order, false);
    let (solved, checked, assigned) = if rev.0 > fwd.0 { rev } else { fwd };
    // Only trust the trail when it explains most of what it could check.
    if solved < 2 || solved * 10 < checked * 6 {
        return false;
    }
    for (i, dir) in assigned {
        txns[i].direction = dir;
    }
    true
}

// --------------------------- Text / PDF path ---------------------------

pub(crate) fn text_to_txns(source: &str, lines: &[String], out: &mut Vec<Transaction>) {
    // If this looks like a structured statement table with wrapped rows
    // (each transaction begins with a date + time, e.g. OPay/PDF exports),
    // reconstruct logical rows and parse the debit/credit/balance triple.
    let mut joined = lines.join("\n");
    if ROW_START.find_iter(&joined).count() >= 5 {
        // Multi-account statement (e.g. Wallet + Savings in one PDF): each account
        // section begins with its own "Total Credit" summary header. Merging
        // accounts double-counts internal transfers (and would even absorb the
        // second account's summary totals into the last row), so keep only the
        // first account by truncating at the start of the second section.
        let low = joined.to_lowercase();
        let mut marks = low.match_indices("total credit");
        if marks.next().is_some() {
            if let Some((mut second, _)) = marks.next() {
                while second > 0 && !joined.is_char_boundary(second) {
                    second -= 1;
                }
                joined.truncate(second);
            }
        }
        let starts: Vec<usize> = ROW_START.find_iter(&joined).map(|m| m.start()).collect();
        for w in 0..starts.len() {
            let s = starts[w];
            let e = if w + 1 < starts.len() { starts[w + 1] } else { joined.len() };
            let row: String = joined[s..e].split_whitespace().collect::<Vec<_>>().join(" ");
            parse_statement_row(source, w, &row, out);
        }
        return;
    }

    // Column statements with proper `.dd` amounts (most bank PDFs).
    let decimal_lines = lines.iter().filter(|l| !strict_amounts(l).is_empty()).count();
    if decimal_lines >= 3 {
        let before = out.len();
        statement_lines_to_txns(source, lines, out);
        if out.len() > before {
            return;
        }
    }
    text_to_txns_lines(source, lines, out);
}

/// Parse one reconstructed statement row: `<datetime> <valuedate> <desc...>
/// <debit> <credit> <balance> <channel> <ref>`. Amounts are `.dd` decimals or `--`.
fn parse_statement_row(source: &str, idx: usize, row: &str, out: &mut Vec<Transaction>) {
    let ms: Vec<regex::Match> = AMT_OR_DASH.find_iter(row).collect();
    let n = ms.len();
    if n < 3 {
        return; // need debit, credit, balance
    }
    // Last three money-or-dash tokens are debit, credit, balance.
    let d_tok = ms[n - 3].as_str();
    let c_tok = ms[n - 2].as_str();
    let cut = ms[n - 3].start();

    let mag = |t: &str| -> Option<f64> {
        if t == "--" { None } else { util::parse_amount(t).map(|m| m.magnitude()).filter(|x| *x > 0.0) }
    };
    let (amount, direction) = match (mag(d_tok), mag(c_tok)) {
        (Some(d), _) => (d, Direction::Debit),
        (None, Some(c)) => (c, Direction::Credit),
        (None, None) => return,
    };

    let date = util::find_date_in_line(row);
    // The row starts with the transaction datetime, so the first clock time is it.
    let time = util::find_time_in_line(row);
    let desc_raw = ROW_PREFIX.replace(&row[..cut], "");
    let desc: String = desc_raw.chars().filter(|c| !"₦$£€¥₹₵₿₩".contains(*c)).collect();
    let desc = clean(&desc);

    out.push(Transaction {
        date,
        time,
        description: if desc.is_empty() { clean(row) } else { desc },
        amount,
        direction,
        balance: mag(ms[n - 1].as_str()),
        raw: clean(row),
        source: source.to_string(),
        line_no: idx,
    });
}

// ---- Column statements: `date  details  paid out  paid in  balance` ----

/// A money token with real `.dd` decimals found in a text line.
struct Amt {
    value: f64,
    negative: bool,
    bytes: std::ops::Range<usize>,
    /// Character columns (not bytes) so they line up with header labels.
    col_start: usize,
    col_end: usize,
}

static STRICT_AMOUNT: Lazy<Regex> = Lazy::new(|| Regex::new(r"\d{1,3}(?:,\d{3})+\.\d{2}|\d+\.\d{2}").unwrap());
static SUMMARY_LINE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?ix)
        \b(?: (?:brought|carried)\s+forward
          | (?:opening|closing|start|starting|end|ending|previous|new|final)\s+balance
          | balance\s+(?:on|at|as\s+at|b/f|c/f)\b
          | total\s+(?:paid|money|payments?|receipts?|debits?|credits?|withdrawals?|deposits?|in\b|out\b|amount)
          | totals?\s*:
        )",
    )
    .unwrap()
});
static OPENING_LINE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(brought\s+forward|b/f|(opening|start|starting|previous)\s+balance)").unwrap());
static NOISE_LINE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?ix)
        overdraft | interest\s+rate | \b(?:aer|ear|apr|gross)\b | sort\s*code | account\s+(?:number|no\b|name)
      | \b(?:iban|bic|swift)\b | page\s+\d+ | \bfscs\b | compensation\s+scheme | statement\s+(?:period|date|number|no\b)
      | \blimit\b | % | protected\s+up\s+to",
    )
    .unwrap()
});

fn strict_amounts(line: &str) -> Vec<Amt> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    for m in STRICT_AMOUNT.find_iter(line) {
        // Reject fragments of longer tokens (references, dates, percentages).
        let before = line[..m.start()].chars().next_back();
        let after = line[m.end()..].chars().next();
        if before.map_or(false, |c| c.is_alphanumeric() || c == '.' || c == ',' || c == '/')
            || after.map_or(false, |c| c.is_ascii_digit() || c == '%' || c == '/')
            || (after == Some('.') && bytes.get(m.end() + 1).map_or(false, |b| b.is_ascii_digit()))
        {
            continue;
        }
        let Some(value) = util::parse_amount(m.as_str()).map(|x| x.magnitude()) else { continue };
        // Sign: "-12.50", "£-12.50", "-£12.50", "12.50-", "12.50 DR", "(12.50)".
        let lead: String = line[..m.start()].chars().rev().take(4).collect();
        let lead = lead.trim_start();
        let mut it = lead.chars();
        let c1 = it.next();
        let c2 = it.clone().find(|c| !c.is_whitespace());
        let is_minus = |c: Option<char>| matches!(c, Some('-') | Some('\u{2212}') | Some('\u{2013}'));
        let is_sym = |c: Option<char>| matches!(c, Some('£') | Some('$') | Some('€') | Some('₦'));
        let tail = &line[m.end()..];
        let tail_trim = tail.trim_start();
        let suffix_neg = tail.starts_with('-')
            || ["DR", "OD"].iter().any(|w| {
                tail_trim.starts_with(w) && tail_trim[w.len()..].chars().next().map_or(true, |c| !c.is_alphanumeric())
            });
        let negative = is_minus(c1) || (is_sym(c1) && is_minus(c2)) || suffix_neg || (c1 == Some('(') && tail.starts_with(')'));
        out.push(Amt {
            value,
            negative,
            bytes: m.range(),
            col_start: line[..m.start()].chars().count(),
            col_end: line[..m.end()].chars().count(),
        });
    }
    out
}

/// Column positions of a statement table header, in character columns.
#[derive(Default, Clone)]
struct TextCols {
    out: Option<(usize, usize)>,
    inn: Option<(usize, usize)>,
    amount: Option<(usize, usize)>,
    balance: Option<(usize, usize)>,
}

#[derive(Clone, Copy, PartialEq)]
enum ColKind {
    Out,
    In,
    Amount,
    Balance,
}

fn find_label(lower: &str, labels: &[&str]) -> Option<(usize, usize)> {
    for lab in labels {
        let mut from = 0;
        while let Some(i) = lower[from..].find(lab) {
            let s = from + i;
            let e = s + lab.len();
            let b = lower[..s].chars().next_back().map_or(true, |c| !c.is_alphanumeric());
            let a = lower[e..].chars().next().map_or(true, |c| !c.is_alphanumeric());
            if b && a {
                return Some((lower[..s].chars().count(), lower[..e].chars().count()));
            }
            from = e;
        }
    }
    None
}

/// Recognise a table header line and, when the text keeps its page layout
/// (labels separated by runs of spaces), remember where each column sits.
fn parse_text_header(line: &str) -> Option<TextCols> {
    if !strict_amounts(line).is_empty() {
        return None;
    }
    let lower = line.to_lowercase();
    let cols = TextCols {
        out: find_label(&lower, &["paid out", "money out", "payments out", "withdrawn", "withdrawals", "debits", "debit", "payments", "out"]),
        inn: find_label(&lower, &["paid in", "money in", "payments in", "deposits", "credits", "credit", "receipts", "in"]),
        amount: find_label(&lower, &["amount"]),
        balance: find_label(&lower, &["balance"]),
    };
    let two_sided = cols.out.is_some() && cols.inn.is_some();
    let signed = cols.amount.is_some() && cols.balance.is_some();
    if !(two_sided || signed) || !(lower.contains("date") || cols.balance.is_some()) {
        return None;
    }
    Some(cols)
}

impl TextCols {
    /// True when the header came from layout-preserving text, so positions mean something.
    fn positional(&self, line: &str) -> bool {
        let spans: Vec<(usize, usize)> = [self.out, self.inn, self.amount, self.balance].into_iter().flatten().collect();
        let lo = spans.iter().map(|s| s.0).min().unwrap_or(0);
        let hi = spans.iter().map(|s| s.1).max().unwrap_or(0);
        let region: String = line.chars().skip(lo).take(hi.saturating_sub(lo)).collect();
        region.contains("  ")
    }

    fn classify(&self, a: &Amt) -> Option<ColKind> {
        let dist = |span: (usize, usize)| -> usize {
            let d = |x: usize, y: usize| x.abs_diff(y);
            d(a.col_end, span.1).min(d(a.col_start, span.0)).min(d((a.col_start + a.col_end) / 2, (span.0 + span.1) / 2))
        };
        [(self.out, ColKind::Out), (self.inn, ColKind::In), (self.amount, ColKind::Amount), (self.balance, ColKind::Balance)]
            .into_iter()
            .filter_map(|(s, k)| s.map(|s| (dist(s), k)))
            .min_by_key(|(d, _)| *d)
            .map(|(_, k)| k)
    }
}

fn statement_lines_to_txns(source: &str, lines: &[String], out: &mut Vec<Transaction>) {
    // Year for dates printed without one ("5 Oct"): taken from the statement period.
    let period_end = lines.iter().take(60).filter_map(|l| util::find_date_in_line(l)).max();
    let fallback_year = period_end.map_or_else(|| chrono::Utc::now().year(), |d| d.year());
    let yearless = |text: &str| -> Option<(NaiveDate, std::ops::Range<usize>)> {
        let (d, at) = util::find_yearless_date(text, fallback_year)?;
        // "28 Dec" on a statement ending in January belongs to the year before.
        let d = match period_end {
            Some(end) if d > end + chrono::Duration::days(31) => d.with_year(d.year() - 1).unwrap_or(d),
            _ => d,
        };
        Some((d, at))
    };

    let first = out.len();
    let mut cols: Option<TextCols> = None;
    // Where the date column sits (from the header), so a date mentioned inside
    // a description ("On 04 Oct") is not mistaken for the row's own date.
    let mut date_col: Option<usize> = None;
    let mut last_date: Option<NaiveDate> = None;
    let mut pending: Vec<String> = Vec::new();
    let mut known_before: std::collections::HashMap<usize, f64> = std::collections::HashMap::new();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(h) = parse_text_header(line) {
            let positional = h.positional(line);
            date_col = if positional { find_label(&line.to_lowercase(), &["date"]).map(|s| s.0) } else { None };
            cols = if positional { Some(h) } else { None };
            pending.clear();
            continue;
        }
        let amts = strict_amounts(line);
        let indent = line.len() - line.trim_start().len();

        // The row's own date: in the date column, or at the very start of the line.
        let in_date_col = |start: usize| -> bool {
            let c = line[..start].chars().count();
            match date_col {
                Some(dc) => c + 4 >= dc && c <= dc + 4,
                None => c <= indent + 2,
            }
        };
        let dated = util::find_date_span(line)
            .filter(|(_, r)| in_date_col(r.start))
            .or_else(|| yearless(line).filter(|(_, r)| in_date_col(r.start)));
        let own_date = dated.as_ref().map(|(d, _)| *d);

        // Balance summaries are not transactions, but an opening balance anchors the trail.
        if SUMMARY_LINE.is_match(line) {
            if OPENING_LINE.is_match(line) {
                // The balance column if we know it, else the first figure after the label.
                let label_end = OPENING_LINE.find(line).map_or(0, |m| m.end());
                let pick = match &cols {
                    Some(c) => amts.iter().find(|a| c.classify(a) == Some(ColKind::Balance)),
                    None => None,
                }
                .or_else(|| amts.iter().find(|a| a.bytes.start >= label_end));
                if let Some(a) = pick {
                    known_before.insert(out.len() - first, if a.negative { -a.value } else { a.value });
                }
            }
            if own_date.is_some() {
                last_date = own_date;
            }
            pending.clear();
            continue;
        }
        if own_date.is_some() {
            last_date = own_date;
        }

        // Text of the line without figures, symbols or its own date.
        let mut text = line.to_string();
        let mut cut: Vec<std::ops::Range<usize>> = amts.iter().map(|a| a.bytes.clone()).collect();
        if let Some((_, r)) = &dated {
            cut.push(r.clone());
        }
        cut.sort_by(|a, b| b.start.cmp(&a.start));
        for r in cut {
            text.replace_range(r, " ");
        }
        let text: String = text.chars().filter(|c| !"₦$£€¥₹₵₿₩".contains(*c)).collect();
        // Drop signs and brackets left behind by the figures we removed.
        let text = text
            .split_whitespace()
            .filter(|w| !w.chars().all(|c| "-\u{2212}\u{2013}()+".contains(c)))
            .collect::<Vec<_>>()
            .join(" ");

        if amts.is_empty() {
            // Wrapped description text: belongs to the neighbouring transaction.
            if last_date.is_some() && text.chars().count() <= 70 && !NOISE_LINE.is_match(line) && text.chars().any(|c| c.is_alphabetic()) {
                if pending.len() < 3 {
                    pending.push(text);
                }
            } else {
                pending.clear();
            }
            continue;
        }

        // Figures before the first dated row (limits, summaries) are not transactions.
        let Some(date) = own_date.or(last_date) else { continue };
        if own_date.is_none() && NOISE_LINE.is_match(line) {
            continue;
        }

        // Which figure is the transaction and which is the running balance?
        let mut txn: Option<(&Amt, Option<Direction>)> = None;
        let mut balance: Option<f64> = None;
        match &cols {
            Some(c) => {
                for a in &amts {
                    match c.classify(a) {
                        Some(ColKind::Balance) => balance = Some(if a.negative { -a.value } else { a.value }),
                        Some(ColKind::Out) if txn.is_none() => txn = Some((a, Some(Direction::Debit))),
                        Some(ColKind::In) if txn.is_none() => txn = Some((a, Some(Direction::Credit))),
                        Some(ColKind::Amount) if txn.is_none() => {
                            txn = Some((a, Some(if a.negative { Direction::Debit } else { Direction::Credit })))
                        }
                        _ => {}
                    }
                }
            }
            None => {
                if amts.len() >= 2 {
                    let b = &amts[amts.len() - 1];
                    balance = Some(if b.negative { -b.value } else { b.value });
                    txn = Some((&amts[amts.len() - 2], None));
                } else {
                    txn = Some((&amts[0], None));
                }
            }
        }
        let Some((a, dir)) = txn else {
            // A balance on its own line: remember it for the trail.
            if let Some(b) = balance {
                known_before.insert(out.len() - first, b);
            }
            continue;
        };
        if a.value == 0.0 {
            continue;
        }
        let direction = dir.unwrap_or_else(|| {
            if a.negative {
                Direction::Debit
            } else {
                classify_text(line, &Money { value: a.value, negative_hint: false })
            }
        });

        // Attach wrapped description lines to the right transaction.
        let mut description = text;
        if !pending.is_empty() {
            let extra = pending.join(" ");
            if own_date.is_some() {
                if let Some(prev) = out[first..].last_mut() {
                    prev.description = clean(&format!("{} {}", prev.description, extra));
                    prev.raw = clean(&format!("{} {}", prev.raw, extra));
                }
            } else {
                description = clean(&format!("{extra} {description}"));
            }
            pending.clear();
        }
        let raw = clean(&format!("{description} {}", clean(line)));
        out.push(Transaction {
            date: Some(date),
            time: util::find_time_in_line(line),
            description: if description.is_empty() { clean(line) } else { description },
            amount: a.value,
            direction,
            balance,
            raw,
            source: source.to_string(),
            line_no: i,
        });
    }
    // Wrapped text after the final row.
    if !pending.is_empty() && pending.len() <= 2 {
        if let Some(prev) = out[first..].last_mut() {
            let extra = pending.join(" ");
            prev.description = clean(&format!("{} {}", prev.description, extra));
            prev.raw = clean(&format!("{} {}", prev.raw, extra));
        }
    }

    // The running balance has the final say on money in vs money out.
    fix_directions_by_balance(&mut out[first..], &known_before);
}

fn text_to_txns_lines(source: &str, lines: &[String], out: &mut Vec<Transaction>) {
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.len() < 6 {
            continue;
        }
        let amts = util::find_amounts_in_line(line);
        if amts.is_empty() {
            continue;
        }
        let date = util::find_date_in_line(line);
        // Heuristic: if >=2 amounts, the last is usually a running balance.
        let (money, amt_range, balance) = if amts.len() >= 2 {
            let bal = amts.last().map(|(m, _)| m.value);
            let pick = &amts[amts.len() - 2];
            (pick.0, pick.1.clone(), bal)
        } else {
            (amts[0].0, amts[0].1.clone(), None)
        };

        let direction = classify_text(line, &money);

        // Description = line with ALL amount tokens stripped (right-to-left
        // so earlier byte ranges stay valid), plus stray currency symbols.
        let _ = &amt_range;
        let mut desc = line.to_string();
        let mut ranges: Vec<std::ops::Range<usize>> = amts.iter().map(|(_, r)| r.clone()).collect();
        ranges.sort_by(|a, b| b.start.cmp(&a.start));
        for r in ranges {
            if r.end <= desc.len() {
                desc.replace_range(r, " ");
            }
        }
        let desc: String = desc.chars().filter(|c| !"₦$£€¥₹₵₿₩".contains(*c)).collect();
        let desc = clean(&desc);
        if desc.is_empty() {
            continue;
        }

        out.push(Transaction {
            date,
            time: util::find_time_in_line(line),
            description: desc,
            amount: money.magnitude(),
            direction,
            balance,
            raw: clean(line),
            source: source.to_string(),
            line_no: i,
        });
    }
}

const DEBIT_WORDS: &[&str] = &[
    "withdrawal", "debit", "transfer to", "payment", "purchase", "charge", "fee", "levy", "vat", "bill",
    "airtime", "outflow", "paid out", "paid to", "sent to", "standing order", "contactless", "cash machine",
];
const CREDIT_WORDS: &[&str] = &[
    "deposit", "credit", "salary", "transfer from", "payment from", "received", "refund", "reversal",
    "inflow", "paid in", "received from", "interest", "income", "lodgement", "cashback",
];
// Short bank codes only count as whole words ("CR" must not match "CROYDON").
const DEBIT_CODES: &[&str] = &["dr", "pos", "atm", "dd", "so", "vis", "deb", "fpo", "cpt", "chq", "chg", "bp"];
const CREDIT_CODES: &[&str] = &["cr", "fpi", "bgc", "dep"];

fn classify_text(line: &str, money: &Money) -> Direction {
    let l = format!(" {} ", line.to_lowercase());
    let words: Vec<&str> = l.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let debit_hit = DEBIT_WORDS.iter().any(|w| l.contains(w)) || DEBIT_CODES.iter().any(|c| words.contains(c));
    let credit_hit = CREDIT_WORDS.iter().any(|w| l.contains(w)) || CREDIT_CODES.iter().any(|c| words.contains(c));
    match (debit_hit, credit_hit) {
        (true, false) => Direction::Debit,
        (false, true) => Direction::Credit,
        _ => {
            // Fall back to sign / parentheses.
            if money.negative_hint || money.value < 0.0 {
                Direction::Debit
            } else {
                Direction::Credit
            }
        }
    }
}

// --------------------------- Statistics ---------------------------

fn side_stats(label: &str, txns: &[&&Transaction], sym: &str) -> SideStats {
    let count = txns.len();
    let total: f64 = txns.iter().map(|t| t.amount).sum();
    let average = if count > 0 { total / count as f64 } else { 0.0 };
    let min = txns.iter().map(|t| t.amount).fold(f64::INFINITY, f64::min);
    let max = txns.iter().map(|t| t.amount).fold(f64::NEG_INFINITY, f64::max);

    let mut dates: Vec<NaiveDate> = txns.iter().filter_map(|t| t.date).collect();
    dates.sort();
    let first = dates.first().copied();
    let last = dates.last().copied();
    let duration = match (first, last) {
        (Some(a), Some(b)) => Some(duration_between(a, b)),
        _ => None,
    };

    SideStats {
        label: label.to_string(),
        total: round2(total),
        total_formatted: fmt_money(sym, total),
        count,
        average: round2(average),
        min: if min.is_finite() { round2(min) } else { 0.0 },
        max: if max.is_finite() { round2(max) } else { 0.0 },
        first_date: first.map(|d| d.to_string()),
        last_date: last.map(|d| d.to_string()),
        duration,
    }
}

fn overall_range(txns: &[&Transaction]) -> (Option<NaiveDate>, Option<NaiveDate>) {
    let mut dates: Vec<NaiveDate> = txns.iter().filter_map(|t| t.date).collect();
    dates.sort();
    (dates.first().copied(), dates.last().copied())
}

fn duration_between(a: NaiveDate, b: NaiveDate) -> DurationBreakdown {
    let (first, last) = if a <= b { (a, b) } else { (b, a) };
    let days = (last - first).num_days();
    let weeks = days / 7;
    let mut months = (last.year() - first.year()) * 12 + (last.month() as i32 - first.month() as i32);
    if last.day() < first.day() {
        months -= 1;
    }
    let months = months.max(0) as i64;
    let years = months / 12;
    let rem_months = months % 12;

    let human = if years >= 1 {
        format!(
            "{years} year{} {rem_months} month{}",
            plural(years),
            plural(rem_months)
        )
    } else if months >= 1 {
        let rem_days = days - months * 30;
        format!("{months} month{} {} day{}", plural(months), rem_days.max(0), plural(rem_days.max(0)))
    } else if weeks >= 1 {
        let rem = days - weeks * 7;
        format!("{weeks} week{} {rem} day{}", plural(weeks), plural(rem))
    } else {
        format!("{days} day{}", plural(days))
    };

    DurationBreakdown { days, weeks, months, years, human }
}

fn plural(n: i64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

// --------------------------- Formatting helpers ---------------------------

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn fmt_money(sym: &str, value: f64) -> String {
    // Guard against non-finite / absurd values so we never render i64::MAX junk.
    if !value.is_finite() || value.abs() >= 1e15 {
        return format!("{sym}{value:.2}");
    }
    let neg = value < 0.0;
    let v = value.abs();
    let whole = v.trunc() as i64;
    let cents = ((v - whole as f64) * 100.0).round() as i64;
    let mut s = String::new();
    let digits = whole.to_string();
    let bytes = digits.as_bytes();
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            s.push(',');
        }
        s.push(*b as char);
    }
    let body = format!("{s}.{:02}", cents);
    if neg {
        format!("-{sym}{body}")
    } else {
        format!("{sym}{body}")
    }
}

fn human_size(bytes: usize) -> String {
    let b = bytes as f64;
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

fn clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").trim().to_string()
}

/// True if any distinctive token (3+ chars) of `kw` appears in the description or raw text.
/// Lenient on purpose: "ndo hotel" should still hit "NDO HOTELS LTD".
fn tokens_any_match(desc: &str, raw: &str, kw: &str) -> bool {
    if kw.is_empty() {
        return true;
    }
    let d = desc.to_lowercase();
    let r = raw.to_lowercase();
    let toks: Vec<&str> = kw.split_whitespace().filter(|w| w.len() >= 3).collect();
    if toks.is_empty() {
        return d.contains(kw) || r.contains(kw);
    }
    toks.iter().any(|w| d.contains(w) || r.contains(w))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn dedup(v: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    v.into_iter().filter(|x| seen.insert(x.clone())).collect()
}

fn error_result(filename: &str, query: &str, size: usize, msg: String) -> AnalysisResult {
    AnalysisResult {
        ok: false,
        query: query.to_string(),
        cache_id: None,
        interpreted: Interpreted {
            keyword: String::new(),
            direction: None,
            date_from: None,
            date_to: None,
            human: String::new(),
            smart: false,
        },
        file: FileMeta {
            name: filename.to_string(),
            kind: "error".into(),
            size_bytes: size,
            size_human: human_size(size),
            parts: vec![],
        },
        currency: crate::currency::detect(""),
        summary: Summary {
            total_transactions_scanned: 0,
            matched_transactions: 0,
            net_amount: 0.0,
            net_formatted: "0.00".into(),
            overall_first_date: None,
            overall_last_date: None,
            overall_duration: None,
        },
        debit: empty_side("Debit"),
        credit: empty_side("Credit"),
        matched: vec![],
        matched_truncated: false,
        warnings: vec![msg],
        elapsed_ms: 0.0,
        throughput_mb_s: 0.0,
    }
}

fn empty_side(label: &str) -> SideStats {
    SideStats {
        label: label.to_string(),
        total: 0.0,
        total_formatted: "0.00".into(),
        count: 0,
        average: 0.0,
        min: 0.0,
        max: 0.0,
        first_date: None,
        last_date: None,
        duration: None,
    }
}

// ============================ tests ============================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Direction;
    use chrono::{Datelike, NaiveDate};

    fn opay_rows(rows: &[&str]) -> String {
        // Wrap each transaction with a datetime prefix so row reconstruction fires.
        let mut s = String::from("Account Statement\nOpening Balance\n");
        for (i, r) in rows.iter().enumerate() {
            let d = (i % 27) + 1;
            s.push_str(&format!("{d:02} Jan 2026 1{}:36:10 {d:02} Jan 2026 {r}\n", i % 6));
        }
        s
    }

    #[test]
    fn csv_debit_credit_columns_and_currency() {
        let csv = "Date,Description,Debit(₦),Credit(₦),Balance\n\
                   2026-01-05,SALARY ACME,,450000.00,450000\n\
                   2026-01-12,POS CHICKEN,12500.50,,437499.50\n";
        let r = run("t.csv", csv.as_bytes(), "");
        assert_eq!(r.currency.code, "NGN");
        assert_eq!(r.credit.count, 1);
        assert_eq!(r.debit.count, 1);
        assert!((r.credit.total - 450000.0).abs() < 0.01);
        assert!((r.debit.total - 12500.50).abs() < 0.01);
    }

    #[test]
    fn account_numbers_not_summed() {
        let csv = "Date,Description,Account,Debit,Credit,Balance\n\
                   2026-01-05,POS,9015413877,5000.00,,0\n\
                   2026-01-06,SALARY,9015413877,,450000.00,0\n";
        let r = run("t.csv", csv.as_bytes(), "");
        assert!((r.debit.total - 5000.0).abs() < 0.01, "debit was {}", r.debit.total);
        assert!((r.credit.total - 450000.0).abs() < 0.01, "credit was {}", r.credit.total);
    }

    #[test]
    fn opay_text_rows_and_reference_fragments_ignored() {
        let txt = opay_rows(&[
            "Betting SPORTYBET 600.00 -- 0.00 Mobile 260101130100878491921527",
            "Transfer from EMEKA -- 3000.00 3000.00 Mobile 000015260103100024803271212087",
            "POS CHICKEN 2500.00 -- 0.00 Mobile 2601031301009250759420",
            "Airtime -- 500.00 500.00 Mobile 2601031301009250759499",
            "Betting 100.00 -- 0.00 Mobile 2601031301009250759477",
            "Refund -- 58.00 58.00 Mobile 2601031301009250759466",
        ]);
        let r = run("s.txt", txt.as_bytes(), "");
        assert_eq!(r.debit.count, 3, "debit count");
        assert_eq!(r.credit.count, 3, "credit count");
        assert!((r.debit.total - 3200.0).abs() < 0.01, "debit total {}", r.debit.total);
        assert!((r.credit.total - 3558.0).abs() < 0.01, "credit total {}", r.credit.total);
    }

    #[test]
    fn huge_number_rejected_and_normal_accepted() {
        assert!(crate::util::parse_amount("9223372036854775807").is_none()); // 19-digit junk
        assert!(crate::util::parse_amount("1,234.56").is_some());
    }

    #[test]
    fn ids_on_a_text_line_are_not_amounts() {
        // A phone/account number and a reference must be ignored; only the real
        // decimal amount survives.
        let toks = crate::util::find_amounts_in_line("Call 08030001234 ref 887766 paid 1,234.56");
        assert_eq!(toks.len(), 1, "tokens: {:?}", toks.iter().map(|(m,_)| m.value).collect::<Vec<_>>());
        assert!((toks[0].0.magnitude() - 1234.56).abs() < 0.01);
    }

    #[test]
    fn currency_alias_needs_word_boundary() {
        // "pancakes"/"cakes" contain "kes" but must NOT trigger Kenyan Shilling.
        let c = crate::currency::detect("Paid ₦1,000 for pancakes and cakes");
        assert_eq!(c.code, "NGN", "detected {:?}", c.code);
    }

    #[test]
    fn nl_query_extracts_keyword_direction_and_range() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let f = crate::nlquery::parse("how much did i spend on chicken last month", today);
        assert_eq!(f.keyword, "chicken");
        assert_eq!(f.direction, Some(Direction::Debit));
        assert!(f.smart);
        assert_eq!(f.date_from.unwrap().month(), 6);
        assert_eq!(f.date_to.unwrap().month(), 6);
    }

    #[test]
    fn empty_query_matches_all() {
        let csv = "Date,Description,Debit,Credit,Balance\n2026-01-01,A,100.00,,0\n2026-01-02,B,,200.00,0\n";
        let r = run("t.csv", csv.as_bytes(), "");
        assert_eq!(r.summary.matched_transactions, 2);
    }

    /// "transfer out of 20k-45k a few hours after I paid ndo hotel 30k":
    /// anchor on the Ndo payment, then only debits in 20k..45k within 6h after it.
    #[test]
    fn anchored_query_with_amount_band_and_time_window() {
        let csv = "Trans. Date,Value Date,Description,Debit(₦),Credit(₦),Balance After(₦)\n\
            05 Mar 2026 09:02:11,05 Mar 2026,Transfer to CHINEDU OKONKWO | OPay,25000.00,--,120000.00\n\
            05 Mar 2026 11:20:14,05 Mar 2026,Transfer to NDO HOTELS LTD | Access Bank,30000.00,--,90000.00\n\
            05 Mar 2026 13:00:40,05 Mar 2026,Transfer to EMEKA NWOSU | OPay,10000.00,--,80000.00\n\
            05 Mar 2026 14:05:52,05 Mar 2026,Transfer to ADEBAYO OLUWASEUN | OPay,35000.00,--,45000.00\n\
            05 Mar 2026 15:10:03,05 Mar 2026,Transfer to FOLASHADE ADEYEMI | GTBank,50000.00,--,0.00\n\
            06 Mar 2026 10:00:00,06 Mar 2026,Transfer to TUNDE BAKARE | OPay,30000.00,--,20000.00\n";
        let r = run("t.csv", csv.as_bytes(), "transfer i made out 20k-45k a few hours after i paid an hotel called ndo 30k");
        assert_eq!(r.summary.matched_transactions, 1, "only the 35k transfer a few hours after Ndo");
        assert!(r.matched[0].description.contains("ADEBAYO"));
        assert_eq!(r.matched[0].time.as_deref(), Some("14:05"));

        // "before" window works too
        let r = run("t.csv", csv.as_bytes(), "what did i send before i paid ndo 30k");
        assert_eq!(r.summary.matched_transactions, 1);
        assert!(r.matched[0].description.contains("CHINEDU"));

        // plain keyword search is unaffected by the new parsing
        let r = run("t.csv", csv.as_bytes(), "adebayo");
        assert_eq!(r.summary.matched_transactions, 1);
    }

    /// "money I sent in the night after I paid ndo 30k on 5 march, 25k-45k":
    /// anchor pinned to a specific day, 18h "night" window crossing midnight,
    /// time-of-day filter keeps the 02:10 transfer and drops the 06:45 one.
    #[test]
    fn anchored_query_specific_day_and_time_of_day() {
        let csv = "Trans. Date,Value Date,Description,Debit(₦),Credit(₦),Balance After(₦)\n\
            01 Mar 2026 10:00:00,01 Mar 2026,Transfer to NDO HOTELS LTD | Access Bank,30000.00,--,300000.00\n\
            01 Mar 2026 22:00:00,01 Mar 2026,Transfer to SOMEONE ELSE | OPay,30000.00,--,270000.00\n\
            05 Mar 2026 21:40:10,05 Mar 2026,Transfer to NDO HOTELS LTD | Access Bank,30000.00,--,170000.00\n\
            06 Mar 2026 02:10:33,06 Mar 2026,Transfer to FOLASHADE ADEYEMI | OPay,40000.00,--,121500.00\n\
            06 Mar 2026 06:45:12,06 Mar 2026,Transfer to ADEBAYO OLUWASEUN | GTBank,35000.00,--,86500.00\n";
        let r = run("t.csv", csv.as_bytes(), "money i sent in the night after i paid ndo 30k on 5 march, 25k-45k");
        assert_eq!(r.summary.matched_transactions, 1, "{:?}", r.matched.iter().map(|m| &m.description).collect::<Vec<_>>());
        assert!(r.matched[0].description.contains("FOLASHADE"));
        assert_eq!(r.matched[0].time.as_deref(), Some("02:10"));
        // the 1 March Ndo payment must NOT have been chosen as the anchor
        assert!(r.warnings.iter().any(|w| w.contains("2026-03-05")));

        // a plain specific-day query works on its own too
        let r = run("t.csv", csv.as_bytes(), "what did i send on 1 march");
        assert_eq!(r.summary.matched_transactions, 2);
    }

    #[test]
    fn amount_band_phrasings() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let f = crate::nlquery::parse("between 20k and 45k", today);
        assert_eq!((f.amount_min, f.amount_max), (Some(20000.0), Some(45000.0)));
        assert!(f.keyword.is_empty(), "amounts must not leak into the keyword");
        let f = crate::nlquery::parse("over 1.5m to chiamaka", today);
        assert_eq!(f.amount_min, Some(1_500_000.0));
        assert_eq!(f.keyword, "chiamaka");
        let f = crate::nlquery::parse("under 5k", today);
        assert_eq!(f.amount_max, Some(5000.0));
    }
}
