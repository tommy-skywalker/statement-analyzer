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
        "csv" => parse_csv(filename, bytes, b','),
        "tsv" => parse_csv(filename, bytes, b'\t'),
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

fn parse_csv(filename: &str, bytes: &[u8], delim: u8) -> Result<Extracted> {
    let mut e = Extracted::empty("csv");
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(delim)
        .from_reader(Cursor::new(bytes));
    let mut rows: Vec<Vec<String>> = Vec::new();
    for rec in rdr.records() {
        match rec {
            Ok(r) => rows.push(r.iter().map(|s| s.to_string()).collect()),
            Err(err) => {
                e.warnings.push(format!("CSV row skipped: {err}"));
            }
        }
    }
    e.blocks.push(Block::Table { source: filename.to_string(), rows });
    Ok(e)
}

fn parse_spreadsheet(filename: &str, bytes: &[u8]) -> Result<Extracted> {
    use calamine::{Data, Reader};
    let mut e = Extracted::empty("spreadsheet");
    let cursor = Cursor::new(bytes.to_vec());
    let mut workbook = calamine::open_workbook_auto_from_rs(cursor)
        .context("could not open spreadsheet")?;
    let sheet_names = workbook.sheet_names().to_vec();
    for name in sheet_names {
        if let Ok(range) = workbook.worksheet_range(&name) {
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
            e.parts.push(name.clone());
            e.blocks.push(Block::Table { source: format!("{filename}#{name}"), rows });
        }
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

fn parse_pdf(filename: &str, bytes: &[u8]) -> Result<Extracted> {
    let mut e = Extracted::empty("pdf");
    match pdf_extract::extract_text_from_mem(bytes) {
        Ok(text) => {
            let lines: Vec<String> = text.lines().map(|l| l.trim_end().to_string()).collect();
            if lines.iter().all(|l| l.trim().is_empty()) {
                e.warnings.push(
                    "PDF produced no extractable text (likely scanned image — OCR not enabled)".into(),
                );
            }
            e.blocks.push(Block::Text { source: filename.to_string(), lines });
        }
        Err(err) => {
            e.warnings.push(format!("PDF text extraction failed: {err}"));
        }
    }
    Ok(e)
}

fn parse_text(filename: &str, bytes: &[u8]) -> Result<Extracted> {
    let mut e = Extracted::empty("text");
    let text = decode_text(bytes);
    let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    e.blocks.push(Block::Text { source: filename.to_string(), lines });
    Ok(e)
}

fn parse_unknown(filename: &str, bytes: &[u8]) -> Result<Extracted> {
    // Generic fallback: if it decodes as mostly-printable text, scan it as text.
    let mut e = Extracted::empty("unknown");
    let text = decode_text(bytes);
    let printable = text.chars().take(4096).filter(|c| !c.is_control() || *c == '\n' || *c == '\t').count();
    let sampled = text.chars().take(4096).count().max(1);
    if printable as f64 / sampled as f64 > 0.85 {
        e.kind = "text(generic)".into();
        let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
        e.blocks.push(Block::Text { source: filename.to_string(), lines });
    } else {
        e.warnings.push(format!(
            "Unsupported binary file type for '{filename}'. Provide CSV, XLSX, PDF, TXT or an archive."
        ));
    }
    Ok(e)
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
