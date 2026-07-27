use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::note::Note;

/// Schema migrations applied in order. Append new migrations to the end —
/// never reorder, never edit a previously released migration in place.
/// `PRAGMA user_version` records the highest version that has been applied,
/// so each fresh DB starts at 0 and walks the list once.
struct Migration {
    version: i32,
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial schema",
        sql: "
            CREATE TABLE IF NOT EXISTS notes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                title TEXT NOT NULL DEFAULT '',
                body TEXT NOT NULL DEFAULT '',
                created_at DATETIME NOT NULL,
                updated_at DATETIME NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_notes_updated ON notes(updated_at DESC);
        ",
    },
    Migration {
        version: 2,
        name: "add pinned column",
        sql: "
            ALTER TABLE notes ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
            CREATE INDEX IF NOT EXISTS idx_notes_pinned ON notes(pinned DESC, updated_at DESC);
        ",
    },
    Migration {
        version: 3,
        name: "add note tags and colors",
        sql: "
            ALTER TABLE notes ADD COLUMN tag TEXT NOT NULL DEFAULT '';
            ALTER TABLE notes ADD COLUMN color TEXT NOT NULL DEFAULT '';
            CREATE INDEX IF NOT EXISTS idx_notes_tag ON notes(tag);
        ",
    },
];

#[derive(Clone)]
pub struct Db {
    inner: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open() -> Result<Self> {
        let path = db_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let conn = Connection::open(&path).with_context(|| format!("open {path:?}"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        run_migrations(&conn)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
        })
    }

    #[cfg(test)]
    fn from_connection(conn: Connection) -> Self {
        Self {
            inner: Arc::new(Mutex::new(conn)),
        }
    }

    pub fn create_note(&self) -> Result<Note> {
        let now = Utc::now();
        let conn = self.inner.lock().unwrap();
        conn.execute(
            "INSERT INTO notes (title, body, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
            params!["", "", now, now],
        )?;
        let id = conn.last_insert_rowid();
        Ok(Note {
            id,
            title: String::new(),
            body: String::new(),
            tag: String::new(),
            color: String::new(),
            created_at: now,
            updated_at: now,
            pinned: false,
        })
    }

    pub fn list_notes(&self) -> Result<Vec<Note>> {
        let conn = self.inner.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, body, tag, color, created_at, updated_at, pinned
             FROM notes
             ORDER BY pinned DESC, updated_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Note {
                id: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
                tag: row.get(3)?,
                color: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
                pinned: row.get::<_, i64>(7)? != 0,
            })
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn set_pinned(&self, id: i64, pinned: bool) -> Result<()> {
        let conn = self.inner.lock().unwrap();
        conn.execute(
            "UPDATE notes SET pinned=?1 WHERE id=?2",
            params![if pinned { 1 } else { 0 }, id],
        )?;
        Ok(())
    }

    pub fn set_tag(&self, id: i64, tag: &str) -> Result<()> {
        let conn = self.inner.lock().unwrap();
        conn.execute("UPDATE notes SET tag=?1 WHERE id=?2", params![tag, id])?;
        Ok(())
    }

    pub fn set_color(&self, id: i64, color: &str) -> Result<()> {
        let conn = self.inner.lock().unwrap();
        conn.execute("UPDATE notes SET color=?1 WHERE id=?2", params![color, id])?;
        Ok(())
    }

    pub fn update_note(&self, id: i64, title: &str, body: &str) -> Result<()> {
        let now = Utc::now();
        let conn = self.inner.lock().unwrap();
        conn.execute(
            "UPDATE notes SET title=?1, body=?2, updated_at=?3 WHERE id=?4",
            params![title, body, now, id],
        )?;
        Ok(())
    }

    pub fn delete_note(&self, id: i64) -> Result<()> {
        let conn = self.inner.lock().unwrap();
        conn.execute("DELETE FROM notes WHERE id=?1", params![id])?;
        Ok(())
    }

    pub fn restore_note(&self, note: &Note) -> Result<()> {
        let conn = self.inner.lock().unwrap();
        conn.execute(
            "INSERT INTO notes (id, title, body, tag, color, created_at, updated_at, pinned)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                note.id,
                note.title,
                note.body,
                note.tag,
                note.color,
                note.created_at,
                note.updated_at,
                if note.pinned { 1 } else { 0 },
            ],
        )?;
        Ok(())
    }

    /// Returns the vacuum key of every image referenced by `![image](...)` in
    /// any note body — see [`image_key`] for what that key is and why it is
    /// not the full path.
    pub fn referenced_image_names(&self) -> Result<HashSet<String>> {
        let conn = self.inner.lock().unwrap();
        let mut stmt = conn.prepare("SELECT body FROM notes")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut refs = HashSet::new();
        for body in rows.flatten() {
            for path in extract_image_paths(&body) {
                if let Some(key) = image_key(&path) {
                    refs.insert(key);
                }
            }
        }
        Ok(refs)
    }

    /// True if any body still carries the raw `![image](` marker. Used as the
    /// orphan-vacuum's regression tripwire: marker present but zero keys
    /// extracted means the parser (or the keying) broke — not that every
    /// image genuinely went unreferenced.
    pub fn any_body_has_image_marker(&self) -> Result<bool> {
        let conn = self.inner.lock().unwrap();
        let mut stmt = conn.prepare("SELECT body FROM notes")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let found = rows.flatten().any(|b| b.contains(IMAGE_MARKER));
        Ok(found)
    }
}

/// Reduce an image reference to the identity the orphan vacuum compares on:
/// the bare file name.
///
/// Bodies store an absolute path (`window::insert_image_at_cursor` writes
/// `path.display()`), so comparing full paths breaks the moment the data
/// directory moves — a `notes.db` carried over from the Linux build has
/// `/home/<u>/.local/share/jot/images/<uuid>.png` in every body while the
/// files on disk live under `%LOCALAPPDATA%\jot\images\`. Zero matches used to
/// mean "delete every image". Both sides are one flat directory, so the file
/// name alone is a sound (and separator-agnostic) identity.
///
/// We split on BOTH separators rather than going through `std::path` because
/// the string may well have been written by the other OS. On Windows the key
/// is lowercased: NTFS resolves names case-insensitively but `str` does not,
/// and a case-only difference must never read as "orphan".
pub(crate) fn image_key(reference: &str) -> Option<String> {
    let name = reference
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())?;
    #[cfg(windows)]
    let name = name.to_ascii_lowercase();
    #[cfg(not(windows))]
    let name = name.to_string();
    Some(name)
}

fn run_migrations(conn: &Connection) -> Result<()> {
    let current: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;

    for m in MIGRATIONS {
        if m.version <= current {
            continue;
        }
        tracing::info!("applying migration {} ({})", m.version, m.name);
        // PRAGMA user_version doesn't accept bound parameters in some SQLite
        // builds, so format it inline. m.version is a hardcoded i32.
        let sql = format!(
            "BEGIN; {}; PRAGMA user_version = {}; COMMIT;",
            m.sql, m.version
        );
        conn.execute_batch(&sql)
            .with_context(|| format!("migration {} ({}) failed", m.version, m.name))?;
    }
    Ok(())
}

/// The literal marker note bodies use for inline images. Shared between the
/// parser below and the orphan-vacuum's regression tripwire in
/// `maintenance.rs` — a guard that hardcoded its own copy would silently stop
/// guarding the moment this string moved.
pub(crate) const IMAGE_MARKER: &str = "![image](";

/// Pull every `path` from `![image](path)` markdown patterns in `body`.
fn extract_image_paths(body: &str) -> Vec<String> {
    let prefix = IMAGE_MARKER.as_bytes();
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(prefix) {
            let after = i + prefix.len();
            if let Some(rel) = bytes[after..].iter().position(|&b| b == b')') {
                let end = after + rel;
                out.push(body[after..end].to_string());
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

pub fn data_dir() -> Result<PathBuf> {
    // Windows: %LOCALAPPDATA%, never %APPDATA%. The roaming profile is copied
    // over SMB at logon/logoff on domain-joined boxes, and this directory holds
    // a WAL-mode SQLite DB, every pasted image and 7 daily .db snapshots —
    // slow logons at best, and WAL over SMB is a known corruption vector.
    #[cfg(windows)]
    let base = dirs::data_local_dir();
    #[cfg(not(windows))]
    let base = dirs::data_dir();
    let dir = base
        .context("could not determine data directory")?
        .join("jot");
    Ok(dir)
}

pub fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("notes.db"))
}

pub fn images_dir() -> Result<PathBuf> {
    let dir = data_dir()?.join("images");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn backups_dir() -> Result<PathBuf> {
    let dir = data_dir()?.join("backups");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::{image_key, run_migrations, Db};
    use rusqlite::Connection;

    #[test]
    fn migration_adds_tag_and_color_columns() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        let mut stmt = conn.prepare("PRAGMA table_info(notes)").unwrap();
        let columns = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(columns.iter().any(|c| c == "tag"));
        assert!(columns.iter().any(|c| c == "color"));

        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 3);
    }

    #[test]
    fn can_persist_and_list_note_tag_and_color() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        let db = Db::from_connection(conn);

        let note = db.create_note().unwrap();
        db.set_tag(note.id, "work").unwrap();
        db.set_color(note.id, "blue").unwrap();

        let notes = db.list_notes().unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].tag, "work");
        assert_eq!(notes[0].color, "blue");
    }

    #[test]
    fn image_key_ignores_the_directory_and_the_separator_flavour() {
        // A body written by the Linux build and one written by the Windows
        // build must reduce to the same key, whichever OS is running the
        // vacuum — otherwise the sweep deletes the images it just failed to
        // recognise.
        let expected = image_key("shot.png");
        assert!(expected.is_some());
        assert_eq!(
            image_key("/home/u/.local/share/jot/images/shot.png"),
            expected
        );
        assert_eq!(
            image_key(r"C:\Users\u\AppData\Local\jot\images\shot.png"),
            expected
        );
        assert_eq!(image_key(""), None);
        assert_eq!(image_key("images/"), None);
    }

    #[test]
    fn referenced_image_names_collect_both_path_flavours() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        let db = Db::from_connection(conn);

        let note = db.create_note().unwrap();
        db.update_note(
            note.id,
            "mixed",
            "before ![image](/home/u/.local/share/jot/images/a.png) middle \
             ![image](C:\\Users\\u\\AppData\\Local\\jot\\images\\b.png) after",
        )
        .unwrap();

        let names = db.referenced_image_names().unwrap();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&image_key("a.png").unwrap()));
        assert!(names.contains(&image_key("b.png").unwrap()));
    }

    #[test]
    fn image_key_case_folds_only_on_windows() {
        // NTFS resolves names case-insensitively, so a case-only difference
        // must never read as "orphan" there; unix filesystems are case-
        // sensitive, so the key must NOT fold.
        #[cfg(windows)]
        assert_eq!(image_key("SHOT.PNG").as_deref(), Some("shot.png"));
        #[cfg(not(windows))]
        assert_eq!(image_key("SHOT.PNG").as_deref(), Some("SHOT.PNG"));
    }

    #[test]
    fn image_marker_tripwire_only_fires_when_bodies_carry_the_marker() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        let db = Db::from_connection(conn);

        // Notes with no image marker at all: the vacuum must keep running
        // (steady state — orphans still get reclaimed).
        let note = db.create_note().unwrap();
        db.update_note(note.id, "plain", "no images here").unwrap();
        assert!(!db.any_body_has_image_marker().unwrap());

        // Marker present: if the parser ever extracts zero keys from this,
        // the tripwire must refuse the sweep.
        db.update_note(note.id, "img", "look ![image](/tmp/x.png)")
            .unwrap();
        assert!(db.any_body_has_image_marker().unwrap());
    }
}
