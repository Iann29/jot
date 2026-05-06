use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::note::Note;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS notes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    created_at DATETIME NOT NULL,
    updated_at DATETIME NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_notes_updated ON notes(updated_at DESC);
";

#[derive(Clone)]
pub struct Db {
    inner: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open() -> Result<Self> {
        let path = data_dir()?.join("notes.db");
        std::fs::create_dir_all(path.parent().unwrap())?;
        let conn = Connection::open(&path).with_context(|| format!("open {path:?}"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
        })
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
            created_at: now,
            updated_at: now,
        })
    }

    pub fn list_notes(&self) -> Result<Vec<Note>> {
        let conn = self.inner.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, body, created_at, updated_at FROM notes ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Note {
                id: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?;
        Ok(rows.filter_map(Result::ok).collect())
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
            "INSERT INTO notes (id, title, body, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![note.id, note.title, note.body, note.created_at, note.updated_at],
        )?;
        Ok(())
    }
}

pub fn data_dir() -> Result<PathBuf> {
    let dir = dirs::data_dir().context("no XDG data dir")?.join("jot");
    Ok(dir)
}

pub fn images_dir() -> Result<PathBuf> {
    let dir = data_dir()?.join("images");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
