//! File-type dispatch. Turns arbitrary uploaded bytes into normalised
//! `Block`s (tabular rows or free-text lines) for the analysis engine.

use anyhow::{Context, Result};
use std::io::Cursor;

/// A unit of extracted content tagged with its origin.
pub enum Block {
    Table { source: String, rows: Vec<Vec<String>> },
    Text { source: String, lines: Vec<String> },
}

pub struct Extracted {
    pub kind: String,
    pub parts: Vec<String>,
    pub blocks: Vec<Block>,
    pub warnings: Vec<String>,
}

impl Extracted {
    fn empty(kind: &str) -> Self {
        Extracted { kind: kind.into(), parts: vec![], blocks: vec![], warnings: vec![] }
    }

    /// Concatenate everything into one text buffer (used for currency detection).
    pub fn raw_text(&self) -> String {
        let mut out = String::new();
        for b in &self.blocks {
            match b {
                Block::Table { rows, .. } => {
                    for r in rows {
                        out.push_str(&r.join(" "));
                        out.push('\n');
                    }
                }
                Block::Text { lines, .. } => {
                    for l in lines {
                        out.push_str(l);
                        out.push('\n');
                    }
                }
            }
        }
        out
    }

    /// Concatenate content but stop after ~`max_bytes` — enough for currency
    /// detection without materialising the whole document (which can be tens
    /// of MB) into a second String.
    pub fn raw_text_sample(&self, max_bytes: usize) -> String {
        let mut out = String::with_capacity(max_bytes.min(1 << 16));
        'outer: for b in &self.blocks {
            match b {
                Block::Table { rows, .. } => {
                    for r in rows {
                        out.push_str(&r.join(" "));
                        out.push('\n');
                        if out.len() >= max_bytes {
                            break 'outer;
                        }
                    }
                }
                Block::Text { lines, .. } => {
                    for l in lines {
                        out.push_str(l);
                        out.push('\n');
                        if out.len() >= max_bytes {
                            break 'outer;
                        }
                    }
                }
            }
        }
        out
    }
}

fn ext_of(filename: &str) -> String {
    filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_lowercase()
}

/// Main entry point. Recursion-safe for archives (depth-limited).
pub fn extract(filename: &str, bytes: &[u8]) -> Result<Extracted> {
    extract_depth(filename, bytes, 0)
}

pub(crate) fn extract_depth(filename: &str, bytes: &[u8], depth: usize) -> Result<Extracted> {
    if depth > 4 {
        let mut e = Extracted::empty("archive");
        e.warnings.push(format!("Skipped {filename}: archive nesting too deep"));
        return Ok(e);
    }

    let ext = ext_of(filename);
    match ext.as_str() {
        "csv" => parse_csv(filename, bytes, None),
        "tsv" => parse_csv(filename, bytes, Some(b'\t')),
        "qif" => parse_qif(filename, &decode_text(bytes)),
        "ofx" | "qfx" => parse_ofx(filename, &decode_text(bytes)),
        "xlsx" | "xlsm" | "xls" | "xlsb" | "ods" => parse_spreadsheet(filename, bytes),
        "pdf" => parse_pdf(filename, bytes),
        "txt" | "log" | "text" | "md" => parse_text(filename, bytes),
        "zip" => crate::parsers::archive::parse_zip(filename, bytes, depth),
        "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" => {
            crate::parsers::archive::parse_external(filename, bytes, depth)
        }
        _ => parse_unknown(filename, bytes),
    }
}

fn parse_csv(filename: &str, bytes: &[u8], delim: Option<u8>) -> Result<Extracted> {
    let mut e = Extracted::empty("csv");
    // Decode first: many bank exports are Windows-1252 (a bare 0xA3 pound sign
    // is invalid UTF-8 and would otherwise make every row unreadable).
    let text = decode_text(bytes);
    let delim = delim.unwrap_or_else(|| sniff_delimiter(&text));
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(delim)
        .from_reader(Cursor::new(text.as_bytes()));
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut skipped = 0usize;
    for rec in rdr.records() {
        match rec {
            Ok(r) => rows.push(r.iter().map(|s| s.to_string()).collect()),
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        e.warnings.push(format!("{skipped} unreadable CSV row(s) were skipped."));
    }
    e.blocks.push(Block::Table { source: filename.to_string(), rows });
    Ok(e)
}

/// Pick the delimiter that splits the first lines most consistently
/// (comma, semicolon, tab or pipe).
fn sniff_delimiter(text: &str) -> u8 {
    let sample: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).take(12).collect();
    let mut best = (b',', 0usize);
    for d in [b',', b';', b'\t', b'|'] {
        let n: usize = sample.iter().map(|l| l.bytes().filter(|b| *b == d).count()).sum();
        if n > best.1 {
            best = (d, n);
        }
    }
    best.0
}

const SYNTH_HEADER: [&str; 4] = ["Date", "Description", "Amount", "Balance"];

/// QIF (Quicken) export: one record per `^`, fields tagged by their first letter.
fn parse_qif(filename: &str, text: &str) -> Result<Extracted> {
    let mut e = Extracted::empty("qif");
    let mut rows: Vec<Vec<String>> = vec![SYNTH_HEADER.iter().map(|s| s.to_string()).collect()];
    let (mut date, mut amount, mut payee, mut memo) = (String::new(), String::new(), String::new(), String::new());
    for line in text.lines() {
        let line = line.trim();
        let Some(tag) = line.chars().next() else { continue };
        let val = line[tag.len_utf8()..].trim();
        match tag {
            'D' => date = val.replace('\'', "/").replace(' ', ""),
            'T' | 'U' => amount = val.to_string(),
            'P' => payee = val.to_string(),
            'M' => memo = val.to_string(),
            '^' => {
                if !amount.is_empty() {
                    let desc = [payee.as_str(), memo.as_str()].iter().filter(|x| !x.is_empty()).cloned().collect::<Vec<_>>().join(" ");
                    rows.push(vec![date.clone(), desc, amount.clone(), String::new()]);
                }
                date.clear();
                amount.clear();
                payee.clear();
                memo.clear();
            }
            _ => {}
        }
    }
    e.blocks.push(Block::Table { source: filename.to_string(), rows });
    Ok(e)
}

/// OFX / QFX export (SGML or XML): read each `<STMTTRN>` block.
fn parse_ofx(filename: &str, text: &str) -> Result<Extracted> {
    let mut e = Extracted::empty("ofx");
    let mut rows: Vec<Vec<String>> = vec![SYNTH_HEADER.iter().map(|s| s.to_string()).collect()];
    let upper = text.to_ascii_uppercase();
    let tag = |block: &str, block_upper: &str, name: &str| -> String {
        let open = format!("<{name}>");
        match block_upper.find(&open) {
            Some(i) => {
                let rest = &block[i + open.len()..];
                let end = rest.find(['<', '\r', '\n']).unwrap_or(rest.len());
                rest[..end].trim().replace("&amp;", "&")
            }
            None => String::new(),
        }
    };
    if let Some(cur) = upper.find("<CURDEF>") {
        let code: String = text[cur + 8..].chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        rows[0][2] = format!("Amount ({code})");
    }
    let mut pos = 0;
    while let Some(i) = upper[pos..].find("<STMTTRN>") {
        let start = pos + i + 9;
        let end = upper[start..].find("<STMTTRN>").map(|j| start + j).unwrap_or(upper.len());
        let (b, bu) = (&text[start..end], &upper[start..end]);
        let date: String = tag(b, bu, "DTPOSTED").chars().take(8).collect();
        let desc = [tag(b, bu, "NAME"), tag(b, bu, "MEMO")].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" ");
        let amount = tag(b, bu, "TRNAMT");
        if !amount.is_empty() {
            rows.push(vec![date, desc, amount, String::new()]);
        }
        pos = end;
    }
    e.blocks.push(Block::Table { source: filename.to_string(), rows });
    Ok(e)
}

/// Text exports made of repeated `Date: / Description: / Amount: / Balance:`
/// blocks (Santander's .txt download). Returns None for ordinary text.
fn parse_key_value_blocks(lines: &[String]) -> Option<Vec<Vec<String>>> {
    let field = |l: &str, key: &str| -> Option<String> {
        let t = l.trim();
        let head = t.get(..key.len())?;
        if head.eq_ignore_ascii_case(key) { Some(t[key.len()..].trim().to_string()) } else { None }
    };
    if lines.iter().filter(|l| field(l, "amount:").is_some()).count() < 3 {
        return None;
    }
    let mut rows: Vec<Vec<String>> = vec![SYNTH_HEADER.iter().map(|s| s.to_string()).collect()];
    let mut cur = vec![String::new(); 4];
    let flush = |cur: &mut Vec<String>, rows: &mut Vec<Vec<String>>| {
        if !cur[2].is_empty() {
            rows.push(cur.clone());
        }
        *cur = vec![String::new(); 4];
    };
    for l in lines {
        if let Some(v) = field(l, "date:") {
            flush(&mut cur, &mut rows);
            cur[0] = v;
        } else if let Some(v) = field(l, "description:") {
            cur[1] = v;
        } else if let Some(v) = field(l, "amount:") {
            cur[2] = v;
        } else if let Some(v) = field(l, "balance:") {
            cur[3] = v;
        }
    }
    flush(&mut cur, &mut rows);
    Some(rows)
}

fn parse_spreadsheet(filename: &str, bytes: &[u8]) -> Result<Extracted> {
    use calamine::{Data, Reader};
    let mut e = Extracted::empty("spreadsheet");
    let cursor = Cursor::new(bytes.to_vec());
    let mut workbook = calamine::open_workbook_auto_from_rs(cursor)
        .context("could not open spreadsheet")?;
    let sheet_names = workbook.sheet_names().to_vec();

    // Collect each non-empty sheet's rows.
    let mut sheets: Vec<(String, Vec<Vec<String>>)> = Vec::new();
    for name in &sheet_names {
        if let Ok(range) = workbook.worksheet_range(name) {
            let mut rows: Vec<Vec<String>> = Vec::with_capacity(range.height());
            for row in range.rows() {
                let cells = row
                    .iter()
                    .map(|c| match c {
                        Data::Empty => String::new(),
                        Data::String(s) => s.clone(),
                        Data::Float(f) => fmt_num(*f),
                        Data::Int(i) => i.to_string(),
                        Data::Bool(b) => b.to_string(),
                        Data::DateTime(d) => d
                            .as_datetime()
                            .map(|dt| dt.format("%Y-%m-%d").to_string())
                            .unwrap_or_else(|| c.to_string()),
                        other => other.to_string(),
                    })
                    .collect();
                rows.push(cells);
            }
            if rows.iter().any(|r| r.iter().any(|c| !c.trim().is_empty())) {
                sheets.push((name.clone(), rows));
            }
        }
    }

    // Multiple sheets usually mean separate accounts (e.g. Wallet vs Savings).
    // Merging them double-counts internal transfers and inflates totals, so we
    // analyse only the primary (first, largest) sheet and note the rest.
    if sheets.len() > 1 {
        sheets.sort_by_key(|(_, r)| std::cmp::Reverse(r.len()));
        let primary = sheets.remove(0);
        let skipped: Vec<String> = sheets.iter().map(|(n, _)| n.clone()).collect();
        e.warnings.push(format!(
            "Workbook has {} sheets; analysing only \"{}\". Skipped (likely separate accounts): {}.",
            skipped.len() + 1,
            primary.0,
            skipped.join(", ")
        ));
        e.parts.push(primary.0.clone());
        e.blocks.push(Block::Table { source: format!("{filename}#{}", primary.0), rows: primary.1 });
    } else if let Some((name, rows)) = sheets.into_iter().next() {
        e.parts.push(name.clone());
        e.blocks.push(Block::Table { source: format!("{filename}#{name}"), rows });
    }

    if e.blocks.is_empty() {
        e.warnings.push("Spreadsheet contained no readable sheets".into());
    }
    Ok(e)
}

fn fmt_num(f: f64) -> String {
    if f.fract() == 0.0 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

/// Wrapped statement tables whose rows start with `DD Mon YYYY HH:MM:SS`
/// (OPay-style). These are rebuilt from the plain text stream.
static DATETIME_ROW: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| regex::Regex::new(r"\d{1,2}\s+[A-Za-z]{3,9}\s+\d{4}\s+\d{1,2}:\d{2}:\d{2}").unwrap());

fn parse_pdf(filename: &str, bytes: &[u8]) -> Result<Extracted> {
    let mut e = Extracted::empty("pdf");
    // The PDF library can panic on unusual fonts/encodings; never let one
    // file take the request (or the server) down with it.
    let plain = pdf_plain_lines(bytes);
    let has_text = |ls: &Vec<String>| ls.iter().any(|l| !l.trim().is_empty());
    let datetime_rows = plain.as_ref().map_or(0, |ls| DATETIME_ROW.find_iter(&ls.join("\n")).count());

    // Column statements (date / paid out / paid in / balance) need the page
    // layout to tell money out from money in, so prefer layout-preserving text.
    let lines = if datetime_rows >= 5 {
        plain
    } else {
        pdf_layout_lines(bytes).filter(has_text).or(plain)
    };
    match lines {
        Some(lines) if has_text(&lines) => e.blocks.push(Block::Text { source: filename.to_string(), lines }),
        Some(_) => e.warnings.push(
            "This PDF has no readable text. It looks like a scanned image or photo; download the statement from your bank as PDF or CSV instead.".into(),
        ),
        None => e.warnings.push(
            "This PDF could not be read. If it is password protected, remove the password or export the statement as CSV.".into(),
        ),
    }
    Ok(e)
}

/// Plain text in content-stream order (pure Rust, always available).
pub(crate) fn pdf_plain_lines(bytes: &[u8]) -> Option<Vec<String>> {
    match std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes)) {
        Ok(Ok(text)) => Some(text.lines().map(|l| l.trim_end().to_string()).collect()),
        _ => None,
    }
}

/// Layout-preserving text via poppler's `pdftotext -layout` when it is
/// installed. Columns stay aligned, which the statement parser relies on.
fn pdf_layout_lines(bytes: &[u8]) -> Option<Vec<String>> {
    use std::io::Write;
    let mut tmp = tempfile::Builder::new().suffix(".pdf").tempfile().ok()?;
    tmp.write_all(bytes).ok()?;
    tmp.flush().ok()?;
    let out = std::process::Command::new("pdftotext")
        .args(["-layout", "-enc", "UTF-8"])
        .arg(tmp.path())
        .arg("-")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(text.lines().map(|l| l.trim_end().replace('\u{c}', "")).collect())
}

fn parse_text(filename: &str, bytes: &[u8]) -> Result<Extracted> {
    let mut e = Extracted::empty("text");
    let text = decode_text(bytes);
    if text.trim_start().starts_with("!Type:") {
        return parse_qif(filename, &text);
    }
    if text.contains("<STMTTRN>") || text.contains("<stmttrn>") {
        return parse_ofx(filename, &text);
    }
    // Non-breaking spaces (common in bank text exports) behave like spaces.
    let lines: Vec<String> = text.lines().map(|l| l.replace('\u{a0}', " ")).collect();
    if let Some(rows) = parse_key_value_blocks(&lines) {
        e.blocks.push(Block::Table { source: filename.to_string(), rows });
        return Ok(e);
    }
    e.blocks.push(Block::Text { source: filename.to_string(), lines });
    Ok(e)
}

fn parse_unknown(filename: &str, bytes: &[u8]) -> Result<Extracted> {
    // Sniff by content first: people rename files and phones drop extensions.
    if bytes.starts_with(b"%PDF") {
        return parse_pdf(filename, bytes);
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return parse_spreadsheet(filename, bytes).or_else(|_| crate::parsers::archive::parse_zip(filename, bytes, 0));
    }
    // Generic fallback: if it decodes as mostly-printable text, scan it as text.
    let text = decode_text(bytes);
    let printable = text.chars().take(4096).filter(|c| !c.is_control() || *c == '\n' || *c == '\r' || *c == '\t').count();
    let sampled = text.chars().take(4096).count().max(1);
    if printable as f64 / sampled as f64 > 0.85 {
        let mut e = parse_text(filename, bytes)?;
        if e.kind == "text" {
            e.kind = "text(generic)".into();
        }
        Ok(e)
    } else {
        let mut e = Extracted::empty("unknown");
        e.warnings.push(format!(
            "Unsupported binary file type for '{filename}'. Provide CSV, XLSX, PDF, TXT or an archive."
        ));
        Ok(e)
    }
}

/// Decode bytes to a String, honouring a UTF-8/UTF-16 BOM, else falling back
/// to a lossy UTF-8 / Windows-1252 decode.
pub fn decode_text(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(&bytes[3..]).into_owned();
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let (cow, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
    cow.into_owned()
}
