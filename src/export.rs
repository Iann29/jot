//! Markdown export: single note or every note. Optionally bundles inline
//! images into a sibling `images/` directory and rewrites paths to be
//! relative, so the export folder is self-contained.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::note::Note;

/// Body parts — the same shape `window::parse_markdown_body` produces.
/// Re-implemented locally to keep this module independent of the editor
/// internals.
enum BodyPart {
    Text(String),
    Image(String),
}

fn parse_markdown_body(body: &str) -> Vec<BodyPart> {
    const PREFIX: &[u8] = b"![image](";
    let bytes = body.as_bytes();
    let mut parts = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(PREFIX) {
            let after = i + PREFIX.len();
            if let Some(rel) = bytes[after..].iter().position(|&b| b == b')') {
                let end = after + rel;
                if !buf.is_empty() {
                    parts.push(BodyPart::Text(std::mem::take(&mut buf)));
                }
                parts.push(BodyPart::Image(body[after..end].to_string()));
                i = end + 1;
                continue;
            }
        }
        let ch = body[i..].chars().next().unwrap();
        buf.push(ch);
        i += ch.len_utf8();
    }
    if !buf.is_empty() {
        parts.push(BodyPart::Text(buf));
    }
    parts
}

pub fn slugify_title(title: &str, fallback_id: i64) -> String {
    let s = slug::slugify(title);
    if s.is_empty() {
        format!("note-{fallback_id}")
    } else {
        s
    }
}

fn yaml_escape(s: &str) -> String {
    if s.chars().any(|c| matches!(c, ':' | '#' | '"' | '\'' | '\n')) {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

fn frontmatter(note: &Note) -> String {
    format!(
        "---\ntitle: {}\nupdated: {}\ncreated: {}\npinned: {}\n---\n\n",
        yaml_escape(&note.title),
        note.updated_at,
        note.created_at,
        note.pinned,
    )
}

fn same_file_contents(a: &Path, b: &Path) -> Result<bool> {
    let am = fs::metadata(a)?;
    let bm = fs::metadata(b)?;
    if am.len() != bm.len() {
        return Ok(false);
    }
    Ok(fs::read(a)? == fs::read(b)?)
}

fn bundle_image(src: &Path, images_dir: &Path) -> Result<String> {
    fs::create_dir_all(images_dir).ok();
    let basename = src
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("image.png");
    let target = images_dir.join(basename);

    let final_name = if target.exists() && !same_file_contents(src, &target)? {
        // Collision with different content — disambiguate via short hash.
        let mut hasher = Sha256::new();
        let mut f = fs::File::open(src)?;
        let mut buf = [0u8; 8192];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let hash = hex::encode(&hasher.finalize()[..8]);
        let stem = Path::new(basename)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("img");
        let ext = Path::new(basename)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("png");
        let name = format!("{stem}-{hash}.{ext}");
        fs::copy(src, images_dir.join(&name))?;
        name
    } else {
        if !target.exists() {
            fs::copy(src, &target)?;
        }
        basename.to_string()
    };
    Ok(format!("images/{final_name}"))
}

fn render_body(note: &Note, dest_dir: &Path, bundle_images: bool) -> Result<String> {
    let parts = parse_markdown_body(&note.body);
    let mut out = String::with_capacity(note.body.len());
    let images_dir = dest_dir.join("images");

    for part in parts {
        match part {
            BodyPart::Text(t) => out.push_str(&t),
            BodyPart::Image(path) => {
                let p = PathBuf::from(&path);
                if bundle_images && p.is_file() {
                    match bundle_image(&p, &images_dir) {
                        Ok(rel) => out.push_str(&format!("![image]({rel})")),
                        Err(_) => out.push_str(&format!("![image]({path})")),
                    }
                } else {
                    out.push_str(&format!("![image]({path})"));
                }
            }
        }
    }
    Ok(out)
}

/// Write one note as a markdown file at `dest_path`. If `bundle_images`,
/// referenced PNGs/etc. are copied to `<dest_path's dir>/images/` and the
/// markdown rewritten to relative paths.
pub fn export_note_md(note: &Note, dest_path: &Path, bundle_images: bool) -> Result<()> {
    let dest_dir = dest_path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dest_dir).ok();

    let body = render_body(note, dest_dir, bundle_images)?;
    let mut file = fs::File::create(dest_path)
        .with_context(|| format!("creating {}", dest_path.display()))?;
    file.write_all(frontmatter(note).as_bytes())?;
    file.write_all(body.as_bytes())?;
    Ok(())
}

/// Write every note in `notes` to `dest_dir/<slug>.md` (sharing one
/// `images/` directory). If `zip_result`, also produce a sibling `.zip`.
pub fn export_all_md(notes: &[Note], dest_dir: &Path, zip_result: bool) -> Result<usize> {
    fs::create_dir_all(dest_dir)?;
    let mut used: HashSet<String> = HashSet::new();
    let mut count = 0;

    for n in notes {
        let mut name = slugify_title(&n.title, n.id);
        if !used.insert(name.clone()) {
            name = format!("{name}-{}", n.id);
            used.insert(name.clone());
        }
        let path = dest_dir.join(format!("{name}.md"));
        export_note_md(n, &path, true)?;
        count += 1;
    }

    if zip_result {
        let zip_path = dest_dir.with_extension("zip");
        zip_dir(dest_dir, &zip_path)?;
    }
    Ok(count)
}

fn zip_dir(src: &Path, zip_path: &Path) -> Result<()> {
    use zip::write::SimpleFileOptions;
    let file = fs::File::create(zip_path)?;
    let mut zw = zip::ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(src)?;
        if path.is_file() {
            zw.start_file(rel.to_string_lossy(), opts)?;
            let mut f = fs::File::open(path)?;
            std::io::copy(&mut f, &mut zw)?;
        } else if !rel.as_os_str().is_empty() {
            zw.add_directory(rel.to_string_lossy(), opts)?;
        }
    }
    zw.finish()?;
    Ok(())
}
