//! The deterministic analysis engine: builds transactions from extracted
//! blocks, classifies debit/credit, and computes per-side statistics.

use crate::extract::{self, Block};
use crate::model::*;
use crate::util::{self, Money};
use chrono::{Datelike, NaiveDate};
use std::time::Instant;

const MATCHED_CAP: usize = 2000;

pub fn run(filename: &str, bytes: &[u8], query: &str) -> AnalysisResult {
    let started = Instant::now();
    let size = bytes.len();

    let extracted = match extract::extract(filename, bytes) {
        Ok(e) => e,
        Err(err) => {
            return error_result(filename, query, size, format!("Extraction failed: {err}"));
        }
    };

    let raw_text = extracted.raw_text();
    let currency = crate::currency::detect(&raw_text);

    let mut warnings = extracted.warnings.clone();
    let mut txns: Vec<Transaction> = Vec::new();
    for block in &extracted.blocks {
        match block {
            Block::Table { source, rows } => table_to_txns(source, rows, &mut txns),
            Block::Text { source, lines } => text_to_txns(source, lines, &mut txns),
        }
    }

    if txns.is_empty() {
        warnings.push(
            "No transactions could be parsed from this file. Check that it contains tabular or line-based statement data."
                .into(),
        );
    }

    // Filter by query (case-insensitive substring). Empty query => match all.
    let q = query.trim().to_lowercase();
    let match_all = q.is_empty();
    if match_all {
        warnings.push("No search term provided — analysing ALL transactions.".into());
    }
    let matched: Vec<&Transaction> = txns
        .iter()
        .filter(|t| match_all || t.description.to_lowercase().contains(&q) || t.raw.to_lowercase().contains(&q))
        .collect();

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
        file: FileMeta {
            name: filename.to_string(),
            kind: extracted.kind,
            size_bytes: size,
            size_human: human_size(size),
            parts: dedup(extracted.parts),
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
    let mut s = 0;
    for c in cells {
        let l = c.to_lowercase();
        let l = l.trim();
        for kw in [
            "date", "description", "narration", "details", "particular", "transaction",
            "debit", "withdrawal", "credit", "deposit", "amount", "balance", "value",
            "reference", "type", "dr", "cr", "memo", "payee",
        ] {
            if l == kw || (l.contains(kw) && l.len() <= kw.len() + 12) {
                s += 1;
                break;
            }
        }
    }
    s
}

fn map_header(cells: &[String]) -> ColMap {
    let mut m = ColMap::default();
    for (i, c) in cells.iter().enumerate() {
        let l = c.to_lowercase();
        let l = l.trim();
        let set = |slot: &mut Option<usize>, idx: usize| {
            if slot.is_none() {
                *slot = Some(idx);
            }
        };
        if l.contains("balance") {
            set(&mut m.balance, i);
        } else if l.contains("withdrawal") || l == "debit" || l == "dr" || l.contains("debit") || l.contains("money out") || l.contains("paid out") {
            set(&mut m.debit, i);
        } else if l.contains("deposit") || l == "credit" || l == "cr" || l.contains("credit") || l.contains("money in") || l.contains("paid in") {
            set(&mut m.credit, i);
        } else if l.contains("date") || l.contains("posted") || l.contains("value date") {
            set(&mut m.date, i);
        } else if l.contains("amount") || l.contains("value") {
            set(&mut m.amount, i);
        } else if l.contains("description") || l.contains("narration") || l.contains("details")
            || l.contains("particular") || l.contains("memo") || l.contains("payee")
            || l.contains("reference") || l.contains("transaction")
        {
            set(&mut m.description, i);
        } else if l == "type" || l.contains("dr/cr") || l.contains("cr/dr") || l.contains("indicator") {
            set(&mut m.type_ind, i);
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
    for (ri, row) in rows.iter().enumerate().skip(start) {
        if row.iter().all(|c| c.trim().is_empty()) {
            continue;
        }
        if let Some(t) = row_to_txn(source, ri, row, &map, ncols) {
            out.push(t);
        }
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

fn row_to_txn(source: &str, line_no: usize, row: &[String], m: &ColMap, _ncols: usize) -> Option<Transaction> {
    let joined = row.join(" ");
    let date = cell(row, m.date)
        .and_then(util::parse_date)
        .or_else(|| util::find_date_in_line(&joined));

    let description = cell(row, m.description)
        .map(|s| s.to_string())
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
        let dir = direction_from_type(cell(row, m.type_ind))
            .unwrap_or_else(|| if a.value < 0.0 || a.negative_hint { Direction::Debit } else { Direction::Credit });
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
    if t == "c" || t == "cr" || t.contains("credit") {
        Some(Direction::Credit)
    } else if t == "d" || t == "dr" || t.contains("debit") {
        Some(Direction::Debit)
    } else {
        None
    }
}

// --------------------------- Text / PDF path ---------------------------

fn text_to_txns(source: &str, lines: &[String], out: &mut Vec<Transaction>) {
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
    "withdrawal", "debit", " dr", "pos ", "atm", "transfer to", "payment", "purchase",
    "charge", "fee", "levy", "vat", "bill", "airtime", "outflow", "paid out", "paid to", "sent to",
];
const CREDIT_WORDS: &[&str] = &[
    "deposit", "credit", " cr", "salary", "transfer from", "received", "refund", "reversal",
    "inflow", "paid in", "received from", "interest", "income", "lodgement",
];

fn classify_text(line: &str, money: &Money) -> Direction {
    let l = format!(" {} ", line.to_lowercase());
    let debit_hit = DEBIT_WORDS.iter().any(|w| l.contains(w));
    let credit_hit = CREDIT_WORDS.iter().any(|w| l.contains(w));
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
