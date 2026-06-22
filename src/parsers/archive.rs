//! Archive extraction.
//! ZIP is handled in pure Rust. RAR / 7z / tar(.gz) are delegated to a system
//! tool (`7z`, `7zz`, `unar`, `unrar`, or `tar`) if one is installed — keeping
//! the binary free of fragile native build dependencies.

use crate::extract::{extract_depth, Block, Extracted};
use anyhow::Result;
use std::io::{Cursor, Read};
use std::process::Command;

const MAX_MEMBER_BYTES: u64 = 512 * 1024 * 1024; // 512 MiB per member guard

pub fn parse_zip(filename: &str, bytes: &[u8], depth: usize) -> Result<Extracted> {
    let mut e = Extracted { kind: "zip".into(), parts: vec![], blocks: vec![], warnings: vec![] };
    let reader = Cursor::new(bytes);
    let mut zip = match zip::ZipArchive::new(reader) {
        Ok(z) => z,
        Err(err) => {
            e.warnings.push(format!("Could not open ZIP: {err}"));
            return Ok(e);
        }
    };

    for i in 0..zip.len() {
        let mut file = match zip.by_index(i) {
            Ok(f) => f,
            Err(err) => {
                e.warnings.push(format!("ZIP member {i} unreadable: {err}"));
                continue;
            }
        };
        if file.is_dir() {
            continue;
        }
        let name = file.name().to_string();
        if file.size() > MAX_MEMBER_BYTES {
            e.warnings.push(format!("ZIP member '{name}' too large, skipped"));
            continue;
        }
        let mut buf = Vec::with_capacity(file.size() as usize);
        if let Err(err) = file.read_to_end(&mut buf) {
            e.warnings.push(format!("ZIP member '{name}' read error: {err}"));
            continue;
        }
        merge_member(&mut e, filename, &name, &buf, depth);
    }
    Ok(e)
}

/// RAR / 7z / tar via an external extractor written to a temp dir.
pub fn parse_external(filename: &str, bytes: &[u8], depth: usize) -> Result<Extracted> {
    let kind = filename.rsplit('.').next().unwrap_or("archive").to_lowercase();
    let mut e = Extracted { kind: kind.clone(), parts: vec![], blocks: vec![], warnings: vec![] };

    let tmp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(err) => {
            e.warnings.push(format!("Could not create temp dir: {err}"));
            return Ok(e);
        }
    };
    let archive_path = tmp.path().join(format!("input.{kind}"));
    if let Err(err) = std::fs::write(&archive_path, bytes) {
        e.warnings.push(format!("Could not stage archive: {err}"));
        return Ok(e);
    }
    let out_dir = tmp.path().join("out");
    let _ = std::fs::create_dir_all(&out_dir);

    let ran = run_extractor(&kind, &archive_path, &out_dir);
    match ran {
        Some(tool) => e.parts.push(format!("extracted with `{tool}`")),
        None => {
            e.warnings.push(format!(
                "No extractor found for .{kind}. Install one of: 7z / 7zz / unar / unrar / tar to enable .{kind} support."
            ));
            return Ok(e);
        }
    }

    // Walk extracted files and recurse.
    let mut stack = vec![out_dir.clone()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(buf) = std::fs::read(&path) {
                let member = path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "member".into());
                merge_member(&mut e, filename, &member, &buf, depth);
            }
        }
    }
    Ok(e)
}

fn run_extractor(kind: &str, archive: &std::path::Path, out: &std::path::Path) -> Option<&'static str> {
    let archive_s = archive.to_string_lossy().to_string();
    let out_s = out.to_string_lossy().to_string();

    // (tool, args) candidate list, tried in order.
    let candidates: Vec<(&'static str, Vec<String>)> = match kind {
        "rar" => vec![
            ("7zz", vec!["x".into(), "-y".into(), format!("-o{out_s}"), archive_s.clone()]),
            ("7z", vec!["x".into(), "-y".into(), format!("-o{out_s}"), archive_s.clone()]),
            ("unar", vec!["-quiet".into(), "-force-overwrite".into(), "-output-directory".into(), out_s.clone(), archive_s.clone()]),
            ("unrar", vec!["x".into(), "-y".into(), archive_s.clone(), format!("{out_s}/")]),
        ],
        "7z" => vec![
            ("7zz", vec!["x".into(), "-y".into(), format!("-o{out_s}"), archive_s.clone()]),
            ("7z", vec!["x".into(), "-y".into(), format!("-o{out_s}"), archive_s.clone()]),
            ("unar", vec!["-quiet".into(), "-force-overwrite".into(), "-output-directory".into(), out_s.clone(), archive_s.clone()]),
        ],
        _ => vec![
            // tar, gz, tgz, bz2
            ("tar", vec!["-xf".into(), archive_s.clone(), "-C".into(), out_s.clone()]),
            ("7zz", vec!["x".into(), "-y".into(), format!("-o{out_s}"), archive_s.clone()]),
            ("7z", vec!["x".into(), "-y".into(), format!("-o{out_s}"), archive_s.clone()]),
        ],
    };

    for (tool, args) in candidates {
        match Command::new(tool).args(&args).output() {
            Ok(o) if o.status.success() => return Some(tool),
            _ => continue,
        }
    }
    None
}

fn merge_member(e: &mut Extracted, archive_name: &str, member: &str, bytes: &[u8], depth: usize) {
    let label = format!("{archive_name}::{member}");
    match extract_depth(&label, bytes, depth + 1) {
        Ok(sub) => {
            for w in sub.warnings {
                e.warnings.push(w);
            }
            for p in sub.parts {
                e.parts.push(p);
            }
            e.parts.push(member.to_string());
            for b in sub.blocks {
                // Re-tag source so the UI can show provenance.
                e.blocks.push(match b {
                    Block::Table { source, rows } => Block::Table { source, rows },
                    Block::Text { source, lines } => Block::Text { source, lines },
                });
            }
        }
        Err(err) => e.warnings.push(format!("Failed to parse '{member}': {err}")),
    }
}
