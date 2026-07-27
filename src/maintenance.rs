//! Background housekeeping: orphan-image vacuum + daily DB backups.

use anyhow::{Context, Result};
use chrono::Local;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

use crate::db::{backups_dir, db_path, image_key, images_dir, Db};

const BACKUP_RETENTION: usize = 7;
const BACKUP_PREFIX: &str = "notes-";
const BACKUP_EXT: &str = "db";

/// Sweep the data directory's `images/` folder and remove any file that no
/// note body references. Cheap to run — just stats every entry once and
/// string-matches against the in-memory ref set.
///
/// Both sides are compared through [`image_key`] (bare file name, case-folded
/// on Windows) rather than as full paths: bodies carry an absolute path from
/// whichever machine pasted the image, and a path mismatch here means mass
/// deletion, not "these are orphans". This runs at every startup, silently.
pub fn vacuum_orphan_images(db: &Db) -> Result<usize> {
    let images = images_dir()?;
    if !images.exists() {
        return Ok(0);
    }

    let referenced = db.referenced_image_names()?;
    // Defence in depth against a future parsing regression: an empty
    // reference set while bodies still CONTAIN the image marker means the
    // parser (or the keying) broke — not that every image went unreferenced.
    // Refuse to empty the folder on that evidence. Keyed on the marker, not
    // on "notes exist": a steady state of notes with no images is ordinary
    // and the vacuum must keep reclaiming orphans there. The is_empty()
    // short-circuit keeps the extra body scan off the common path.
    if referenced.is_empty() && db.any_body_has_image_marker()? {
        tracing::warn!(
            "orphan-image vacuum skipped: note bodies carry image references but none could be parsed"
        );
        return Ok(0);
    }

    let mut removed = 0;
    for entry in std::fs::read_dir(&images).context("read images dir")? {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("read_dir entry: {e}");
                continue;
            }
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // A name we cannot key (non-UTF-8) is one we could never have written,
        // but we also cannot prove it is an orphan. Leave it alone.
        let Some(key) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(image_key)
        else {
            continue;
        };
        if referenced.contains(&key) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                removed += 1;
                tracing::debug!("removed orphan image {}", path.display());
            }
            Err(e) => tracing::warn!("could not remove {}: {e}", path.display()),
        }
    }
    if removed > 0 {
        tracing::info!("vacuumed {removed} orphan image(s)");
    }
    Ok(removed)
}

/// Snapshot the SQLite DB into `<data dir>/backups/notes-YYYY-MM-DD.db` once
/// per calendar day. Older backups beyond `BACKUP_RETENTION` are deleted.
/// Uses the SQLite Online Backup API so it stays consistent even with WAL.
pub fn ensure_daily_backup() -> Result<Option<PathBuf>> {
    let src = db_path()?;
    if !src.exists() {
        return Ok(None);
    }

    let dir = backups_dir()?;
    let today = Local::now().format("%Y-%m-%d").to_string();
    let dest = dir.join(format!("{BACKUP_PREFIX}{today}.{BACKUP_EXT}"));
    if dest.exists() {
        return Ok(None); // already taken today
    }

    backup_via_sqlite(&src, &dest)
        .with_context(|| format!("backup {} -> {}", src.display(), dest.display()))?;
    tracing::info!("daily backup written to {}", dest.display());

    if let Err(e) = rotate_backups(&dir) {
        tracing::warn!("backup rotation: {e}");
    }
    Ok(Some(dest))
}

fn backup_via_sqlite(src: &Path, dest: &Path) -> Result<()> {
    let src_conn = Connection::open(src)?;
    let mut dest_conn = Connection::open(dest)?;
    let backup = rusqlite::backup::Backup::new(&src_conn, &mut dest_conn)?;
    backup.run_to_completion(64, std::time::Duration::from_millis(50), None)?;
    Ok(())
}

fn rotate_backups(dir: &Path) -> Result<()> {
    let mut backups: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(BACKUP_PREFIX) && n.ends_with(BACKUP_EXT))
        })
        .collect();

    // Filenames embed the ISO date, so lexical sort == chronological sort.
    backups.sort();

    while backups.len() > BACKUP_RETENTION {
        let oldest = backups.remove(0);
        if let Err(e) = std::fs::remove_file(&oldest) {
            tracing::warn!("could not remove old backup {}: {e}", oldest.display());
        } else {
            tracing::debug!("rotated out backup {}", oldest.display());
        }
    }
    Ok(())
}
