//! Back up sessions to a zip (raw transcripts + sidechain dirs + manifest) and restore them.
//!
//! Archive layout mirrors the config dir so a restore is a plain copy:
//!   manifest.json
//!   projects/<encoded-cwd>/<session-id>.jsonl
//!   projects/<encoded-cwd>/<session-id>/**        (subagents, tool-results — if present)

use crate::Session;
use chrono::{Datelike, Local, Timelike};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

#[derive(Debug, Default, Clone)]
pub struct BackupReport {
    pub sessions: usize,
    pub files: usize,
    pub bytes: u64,
    pub path: PathBuf,
}

pub fn default_backup_name() -> String {
    format!("claude-sessions-backup-{}.zip", Local::now().format("%Y%m%d-%H%M"))
}

fn add_file(zw: &mut ZipWriter<fs::File>, disk: &Path, arc: &str, opts: SimpleFileOptions, rep: &mut BackupReport) -> io::Result<()> {
    let mut f = fs::File::open(disk)?;
    zw.start_file(arc, opts)?;
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        zw.write_all(&buf[..n])?;
        rep.bytes += n as u64;
    }
    rep.files += 1;
    Ok(())
}

fn add_dir(zw: &mut ZipWriter<fs::File>, disk: &Path, arc: &str, opts: SimpleFileOptions, rep: &mut BackupReport) -> io::Result<()> {
    for e in fs::read_dir(disk)?.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let a = format!("{arc}/{name}");
        if p.is_dir() {
            add_dir(zw, &p, &a, opts, rep)?;
        } else if p.is_file() {
            add_file(zw, &p, &a, opts, rep)?;
        }
    }
    Ok(())
}

/// Write the given sessions into a zip at `out`.
pub fn backup(sessions: &[&Session], out: &Path) -> io::Result<BackupReport> {
    let file = fs::File::create(out)?;
    let mut zw = ZipWriter::new(file);
    let now = Local::now();
    let stamp = zip::DateTime::from_date_and_time(
        now.year() as u16, now.month() as u8, now.day() as u8, now.hour() as u8, now.minute() as u8, now.second() as u8,
    )
    .unwrap_or_default();
    let opts = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(stamp)
        .large_file(true);
    let mut rep = BackupReport { path: out.to_path_buf(), ..Default::default() };

    let manifest = serde_json::json!({
        "tool": "claude-sessions",
        "created": Local::now().to_rfc3339(),
        "sessions": sessions.iter().map(|s| serde_json::json!({
            "id": s.id, "label": s.label(), "cwd": s.cwd, "project_dir": s.path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy()),
            "last_active": s.last_active.map(|t| t.to_rfc3339()),
        })).collect::<Vec<_>>(),
    });
    zw.start_file("manifest.json", opts)?;
    zw.write_all(serde_json::to_string_pretty(&manifest).unwrap().as_bytes())?;

    for s in sessions {
        let proj = s.path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let arc = format!("projects/{proj}/{}.jsonl", s.id);
        add_file(&mut zw, &s.path, &arc, opts, &mut rep)?;
        let side = s.side_dir();
        if side.is_dir() {
            add_dir(&mut zw, &side, &format!("projects/{proj}/{}", s.id), opts, &mut rep)?;
        }
        rep.sessions += 1;
    }
    zw.finish()?;
    Ok(rep)
}

#[derive(Debug, Default, Clone)]
pub struct RestoreReport {
    pub restored: usize,
    pub skipped_existing: usize,
    pub sessions: usize,
}

/// Restore `projects/**` entries from a backup zip into `cfg`. Existing files are
/// left alone unless `overwrite` is set.
pub fn restore(zip_path: &Path, cfg: &Path, overwrite: bool) -> io::Result<RestoreReport> {
    let file = fs::File::open(zip_path)?;
    let mut za = ZipArchive::new(file)?;
    let mut rep = RestoreReport::default();
    for i in 0..za.len() {
        let mut entry = za.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let Some(rel) = entry.enclosed_name() else { continue };
        if !rel.starts_with("projects") {
            continue;
        }
        let dest = cfg.join(&rel);
        if rel.extension().map_or(false, |e| e == "jsonl") && rel.components().count() == 3 {
            rep.sessions += 1;
        }
        if dest.exists() && !overwrite {
            rep.skipped_existing += 1;
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = fs::File::create(&dest)?;
        io::copy(&mut entry, &mut out)?;
        rep.restored += 1;
    }
    Ok(rep)
}
