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

/// Cap for the slug (the `.md` stem). `Note::derive_title` already trims at 80
/// chars, but the export path is `<user-chosen folder>/jot-export-YYYY-MM-DD/
/// <slug>.md`, and Windows' classic 260-char MAX_PATH is easy to blow through
/// from a deep `Documents` tree. 64 leaves comfortable headroom.
const MAX_SLUG_LEN: usize = 64;

/// Names that Windows resolves to a DOS device *before* looking at the
/// extension, so `con.md` opens the console instead of creating a file:
/// `File::create` succeeds, the bytes vanish, and the export reports success.
#[cfg(windows)]
const RESERVED_DOS_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

pub fn slugify_title(title: &str, fallback_id: i64) -> String {
    let mut s = slug::slugify(title);
    if s.len() > MAX_SLUG_LEN {
        // slugify only ever emits ASCII `[a-z0-9-]`, so a byte truncate lands
        // on a char boundary; the trailing-dash trim is cosmetic.
        s.truncate(MAX_SLUG_LEN);
        while s.ends_with('-') {
            s.pop();
        }
    }
    if s.is_empty() {
        return format!("note-{fallback_id}");
    }
    // Step off the device namespace by suffixing the note id — the result is
    // still stable and still recognisable.
    #[cfg(windows)]
    if RESERVED_DOS_NAMES.contains(&s.as_str()) {
        s = format!("{s}-{fallback_id}");
    }
    s
}

fn yaml_escape(s: &str) -> String {
    if s.chars()
        .any(|c| matches!(c, ':' | '#' | '"' | '\'' | '\n'))
    {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

fn frontmatter(note: &Note) -> String {
    format!(
        "---\ntitle: {}\ntag: {}\ncolor: {}\nupdated: {}\ncreated: {}\npinned: {}\n---\n\n",
        yaml_escape(&note.title),
        yaml_escape(&note.tag),
        yaml_escape(&note.color),
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

/// Markdown link targets must use forward slashes. `\` is markdown's escape
/// character, so `![image](C:\Users\...)` renders broken in every viewer and
/// sequences like `\U` are eaten outright. No-op on paths that already use `/`.
fn md_path(p: &str) -> String {
    p.replace('\\', "/")
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
                        // `bundle_image` already returns `images/<name>`.
                        Ok(rel) => out.push_str(&format!("![image]({rel})")),
                        Err(_) => out.push_str(&format!("![image]({})", md_path(&path))),
                    }
                } else {
                    out.push_str(&format!("![image]({})", md_path(&path)));
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
    let mut file =
        fs::File::create(dest_path).with_context(|| format!("creating {}", dest_path.display()))?;
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
        // One unwritable note (a name the OS refuses, a permission error, a
        // path past MAX_PATH) must not abort the batch — propagating here used
        // to drop every note *after* the offender on the floor while the toast
        // said "Export failed", with no hint that a partial export happened.
        match export_note_md(n, &path, true) {
            Ok(()) => count += 1,
            Err(e) => tracing::warn!("export {}: {e:#}", path.display()),
        }
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
        // `*_from_path` routes through zip's `path_to_string`, which joins the
        // components with '/' as APPNOTE 4.4.17.1 requires. The plain
        // `start_file`/`add_directory` take the name verbatim, so on Windows
        // they would emit `images\x.png` — extractors either create a file
        // literally called that or reject the archive.
        if path.is_file() {
            zw.start_file_from_path(rel, opts)?;
            let mut f = fs::File::open(path)?;
            std::io::copy(&mut f, &mut zw)?;
        } else if !rel.as_os_str().is_empty() {
            zw.add_directory_from_path(rel, opts)?;
        }
    }
    zw.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{frontmatter, md_path, slugify_title, zip_dir, MAX_SLUG_LEN};
    use crate::note::Note;
    use chrono::Utc;
    use std::fs;
    use std::path::PathBuf;

    /// Private scratch dir. We have no `tempfile` dev-dependency and adding one
    /// is out of scope, so build a unique path by hand and clean it up.
    fn scratch_dir(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("jot-export-test-{tag}-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn frontmatter_includes_tag_and_color() {
        let note = Note {
            id: 42,
            title: "Deploy".to_string(),
            body: String::new(),
            tag: "work".to_string(),
            color: "purple".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            pinned: true,
        };

        let fm = frontmatter(&note);
        assert!(fm.contains("tag: work\n"));
        assert!(fm.contains("color: purple\n"));
        assert!(fm.contains("pinned: true\n"));
    }

    #[test]
    fn zip_entry_names_use_forward_slashes() {
        let root = scratch_dir("zip");
        let src = root.join("jot-export-2026-07-27");
        fs::create_dir_all(src.join("images")).unwrap();
        fs::write(src.join("note.md"), b"# hi").unwrap();
        fs::write(src.join("images").join("x.png"), b"\x89PNG").unwrap();

        let zip_path = root.join("jot-export-2026-07-27.zip");
        zip_dir(&src, &zip_path).unwrap();

        let archive = zip::ZipArchive::new(fs::File::open(&zip_path).unwrap()).unwrap();
        let names: Vec<String> = archive.file_names().map(str::to_string).collect();

        assert!(
            names.iter().any(|n| n == "images/x.png"),
            "nested entry must be `images/x.png`, got {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains('\\')),
            "no entry may carry a backslash, got {names:?}"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn slug_is_truncated_and_never_bare() {
        let long = "a".repeat(200);
        let slug = slugify_title(&long, 7);
        assert!(slug.len() <= MAX_SLUG_LEN, "{slug}");
        assert!(!slug.ends_with('-'), "{slug}");

        // Nothing slug-able at all falls back to the note id.
        assert_eq!(slugify_title("!!!", 7), "note-7");
    }

    #[test]
    fn md_path_normalises_windows_separators() {
        assert_eq!(
            md_path(r"C:\Users\u\AppData\Local\jot\images\a.png"),
            "C:/Users/u/AppData/Local/jot/images/a.png"
        );
        assert_eq!(
            md_path("/home/u/.local/share/jot/images/a.png"),
            "/home/u/.local/share/jot/images/a.png"
        );
    }
}
