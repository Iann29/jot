use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub struct Note {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Note {
    pub fn derive_title(body: &str) -> String {
        let first = body.lines().next().unwrap_or("").trim();
        if first.is_empty() {
            String::new()
        } else {
            first.chars().take(80).collect()
        }
    }

    pub fn display_title(&self) -> String {
        if self.title.trim().is_empty() {
            "Untitled note".to_string()
        } else {
            self.title.clone()
        }
    }

    pub fn snippet(&self) -> String {
        let mut lines = self
            .body
            .lines()
            .skip(1)
            .map(str::trim)
            .filter(|l| !l.is_empty());
        match lines.next() {
            Some(line) => line.chars().take(120).collect(),
            None => String::new(),
        }
    }

    pub fn matches(&self, needle: &str) -> bool {
        let n = needle.to_lowercase();
        self.title.to_lowercase().contains(&n) || self.body.to_lowercase().contains(&n)
    }
}
