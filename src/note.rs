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
            let cleaned = strip_markdown_markers(trimmed);
            if cleaned.is_empty() {
                continue;
            }
            return cleaned.chars().take(80).collect();
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
            let cleaned = strip_markdown_markers(trimmed);
            if cleaned.is_empty() {
                continue;
            }
            return cleaned.chars().take(120).collect();
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

/// Strip the most common markdown decorations so the sidebar shows a
/// clean title — `**foo**`, `*foo*`, `# foo` all collapse to `foo`.
/// Not a parser, just cheap transforms covering 99% of titles.
fn strip_markdown_markers(s: &str) -> String {
    let mut s = s.trim().to_string();
    // Leading "# " / "## " / "### "
    while let Some(rest) = s.strip_prefix('#') {
        s = rest.to_string();
    }
    if let Some(rest) = s.strip_prefix(' ') {
        s = rest.to_string();
    }
    // Leading "- " / "* " bullets (must check BEFORE the inline strip so a
    // line like "- foo" doesn't lose only its leading "- " here).
    if let Some(rest) = s.strip_prefix("- ").or_else(|| s.strip_prefix("* ")) {
        s = rest.to_string();
    }
    // Bold + code first (paired, two chars vs one).
    s = s.replace("**", "").replace('`', "");
    // Italic — cheap pair strip: surrounding * / _.
    while (s.starts_with('*') && s.ends_with('*')) || (s.starts_with('_') && s.ends_with('_')) {
        if s.len() < 2 {
            break;
        }
        s = s[1..s.len() - 1].to_string();
    }
    s.trim().to_string()
}
