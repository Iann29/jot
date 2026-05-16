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

    /// Returns every image path referenced by `![image](...)` in any note body.
    /// Used by the orphan-image vacuum.
    pub fn referenced_image_paths(&self) -> Result<HashSet<PathBuf>> {
        let conn = self.inner.lock().unwrap();
        let mut stmt = conn.prepare("SELECT body FROM notes")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut refs = HashSet::new();
        for body in rows.flatten() {
            for path in extract_image_paths(&body) {
                refs.insert(PathBuf::from(path));
            }
        }
        Ok(refs)
    }
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

/// Pull every `path` from `![image](path)` markdown patterns in `body`.
fn extract_image_paths(body: &str) -> Vec<String> {
    const PREFIX: &[u8] = b"![image](";
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(PREFIX) {
            let after = i + PREFIX.len();
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
    let dir = dirs::data_dir().context("no XDG data dir")?.join("jot");
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
    use super::{run_migrations, Db};
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
}
