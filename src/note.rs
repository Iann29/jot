use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub struct Note {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub pinned: bool,
}

impl Note {
    pub fn derive_title(body: &str) -> String {
        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || is_image_line(trimmed) {
                continue;
            }
            return trimmed.chars().take(80).collect();
        }
        String::new()
    }

    pub fn display_title(&self) -> String {
        if self.title.trim().is_empty() {
            "Untitled note".to_string()
        } else {
            self.title.clone()
        }
    }

    pub fn snippet(&self) -> String {
        let mut found_title = false;
        for line in self.body.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || is_image_line(trimmed) {
                continue;
            }
            if !found_title {
                found_title = true;
                continue;
            }
            return trimmed.chars().take(120).collect();
        }
        String::new()
    }

    pub fn matches(&self, needle: &str) -> bool {
        let n = needle.to_lowercase();
        self.title.to_lowercase().contains(&n) || self.body.to_lowercase().contains(&n)
    }
}

fn is_image_line(line: &str) -> bool {
    line.starts_with("![image](") && line.ends_with(')')
}
