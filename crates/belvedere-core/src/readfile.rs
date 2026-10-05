//! Reading a file's text for chat: plain text, Markdown and the like,
//! PDF, and Word documents. Read-only, home folder only, never a
//! credential location. Long texts are handed out in parts.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::files;

/// Most bytes read from one file.
pub const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Characters per part handed to the model.
pub const PART_CHARS: usize = 6_000;

/// A file's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub path: String,
    /// `text`, `markdown`, `pdf`, `docx`, ...
    pub kind: String,
    pub text: String,
    /// Pages, for PDFs.
    pub pages: Option<usize>,
}

impl Document {
    /// How many parts of `PART_CHARS` the text makes.
    pub fn parts(&self) -> usize {
        parts_of(&self.text)
    }

    /// One part (1-based), with the text cut at a line break where possible.
    pub fn part(&self, n: usize) -> Option<String> {
        part_of(&self.text, n)
    }
}

pub fn parts_of(text: &str) -> usize {
    let chars = text.chars().count();
    chars.div_ceil(PART_CHARS).max(1)
}

pub fn part_of(text: &str, n: usize) -> Option<String> {
    if n == 0 || n > parts_of(text) {
        return None;
    }
    let chars: Vec<char> = text.chars().collect();
    let start = (n - 1) * PART_CHARS;
    let end = (start + PART_CHARS).min(chars.len());
    Some(chars[start..end].iter().collect())
}

/// Text file kinds by extension.
fn text_kind(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "txt" | "text" | "log" | "csv" | "tsv" | "ini" | "cfg" | "conf" => "text",
        "md" | "markdown" => "markdown",
        "json" | "toml" | "yaml" | "yml" | "xml" | "html" | "htm" => "text",
        "rs" | "py" | "js" | "ts" | "go" | "c" | "h" | "cpp" | "java" | "rb" | "sh" | "sql"
        | "css" => "code",
        _ => return None,
    })
}

/// Reads a file's text. Refuses paths outside the home folder, credential
/// locations, very large files, and kinds it cannot read (images, scanned
/// PDFs without a text layer).
pub fn read(home: &Path, path: &str, extra_excludes: &[String]) -> Result<Document, String> {
    let p = PathBuf::from(path);
    let p = if p.is_absolute() { p } else { home.join(p) };
    let canonical = p
        .canonicalize()
        .map_err(|_| format!("there is no file at {}", p.display()))?;
    let home_canonical = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    if !canonical.starts_with(&home_canonical) {
        return Err("only files in your home folder can be read".into());
    }
    if files::is_credential(&home_canonical, &canonical, extra_excludes) {
        return Err("that is a place for passwords and keys; Belvedere never reads it".into());
    }
    let meta = std::fs::metadata(&canonical).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err(format!("{} is not a file", canonical.display()));
    }
    if meta.len() > MAX_BYTES {
        return Err(format!(
            "{} is {} MB; files over {} MB are not read",
            canonical.display(),
            meta.len() / (1024 * 1024),
            MAX_BYTES / (1024 * 1024)
        ));
    }
    let ext = canonical
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let shown = canonical.to_string_lossy().into_owned();
    match ext.as_str() {
        "pdf" => {
            let pages = pdf_extract::extract_text_by_pages(&canonical)
                .map_err(|e| format!("could not read the PDF: {e}"))?;
            let count = pages.len();
            let text = tidy(&pages.join("\n\n"));
            if text.trim().is_empty() {
                return Err("this PDF has no text layer (a scan); reading scans is not supported".into());
            }
            Ok(Document {
                path: shown,
                kind: "pdf".into(),
                text,
                pages: Some(count),
            })
        }
        "docx" => {
            let text = docx_text(&canonical)?;
            Ok(Document {
                path: shown,
                kind: "docx".into(),
                text,
                pages: None,
            })
        }
        other => match text_kind(other) {
            Some(kind) => {
                let bytes = std::fs::read(&canonical).map_err(|e| e.to_string())?;
                Ok(Document {
                    path: shown,
                    kind: kind.into(),
                    text: tidy(&String::from_utf8_lossy(&bytes)),
                    pages: None,
                })
            }
            None => Err(format!(
                "files of type .{other} are not readable here (plain text, Markdown, PDF, and Word documents are)"
            )),
        },
    }
}

/// The paragraphs of a Word document's main part.
fn docx_text(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("not a Word document: {e}"))?;
    let mut xml = String::new();
    archive
        .by_name("word/document.xml")
        .map_err(|_| "not a Word document (no document.xml)".to_string())?
        .read_to_string(&mut xml)
        .map_err(|e| e.to_string())?;
    Ok(tidy(&strip_docx_xml(&xml)))
}

/// Turns the document XML into text: paragraphs and breaks become line
/// breaks, tabs become tabs, tags go.
fn strip_docx_xml(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len() / 4);
    let mut rest = xml;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let after = &rest[lt..];
        let Some(gt) = after.find('>') else { break };
        let tag = &after[1..gt];
        let name = tag
            .trim_start_matches('/')
            .split([' ', '/'])
            .next()
            .unwrap_or("");
        match name {
            "w:p" if tag.starts_with('/') => out.push('\n'),
            "w:br" | "w:cr" => out.push('\n'),
            "w:tab" => out.push('\t'),
            _ => {}
        }
        rest = &after[gt + 1..];
    }
    out.push_str(rest);
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// Collapses runs of blank lines and trailing spaces.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank += 1;
            if blank <= 1 {
                out.push('\n');
            }
        } else {
            blank = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
pub mod fixtures {
    //! Made-up documents for tests.

    use std::io::Write;
    use std::path::Path;

    /// A real (uncompressed) PDF with one line of text per page.
    pub fn pdf(path: &Path, pages: &[&str]) {
        let mut objects: Vec<String> = Vec::new();
        let n = pages.len();
        // 1: catalog, 2: pages, 3: font, then page/content pairs.
        objects.push("<< /Type /Catalog /Pages 2 0 R >>".into());
        let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 4 + i * 2)).collect();
        objects.push(format!(
            "<< /Type /Pages /Kids [{}] /Count {n} >>",
            kids.join(" ")
        ));
        objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into());
        for (i, text) in pages.iter().enumerate() {
            let content = format!(
                "BT /F1 12 Tf 72 720 Td ({}) Tj ET",
                text.replace(['(', ')', '\\'], " ")
            );
            objects.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
                5 + i * 2
            ));
            objects.push(format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ));
        }
        let mut out = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, obj) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.push_str(&format!("{} 0 obj\n{obj}\nendobj\n", i + 1));
        }
        let xref = out.len();
        out.push_str(&format!(
            "xref\n0 {}\n0000000000 65535 f \n",
            objects.len() + 1
        ));
        for o in offsets {
            out.push_str(&format!("{o:010} 00000 n \n"));
        }
        out.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        ));
        std::fs::write(path, out).unwrap();
    }

    /// A Word document with the given paragraphs.
    pub fn docx(path: &Path, paragraphs: &[&str]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("[Content_Types].xml", opts).unwrap();
        zip.write_all(b"<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"/>").unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        let body: String = paragraphs
            .iter()
            .map(|p| {
                format!(
                    "<w:p><w:r><w:t>{}</w:t></w:r></w:p>",
                    p.replace('&', "&amp;")
                )
            })
            .collect();
        zip.write_all(format!("<?xml version=\"1.0\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body>{body}</w:body></w:document>").as_bytes()).unwrap();
        zip.finish().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn fingerprint(p: &Path) -> (Vec<u8>, std::time::SystemTime) {
        let meta = std::fs::metadata(p).unwrap();
        (std::fs::read(p).unwrap(), meta.modified().unwrap())
    }

    #[test]
    fn reads_text_markdown_pdf_and_docx_without_changing_them() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join("Documents")).unwrap();
        std::fs::write(
            home.join("Documents/notes.md"),
            "# Lease\n\nRent is $1,900 a month.\n\n\n\nEnds June 30, 2027.\n",
        )
        .unwrap();
        pdf(
            &home.join("Documents/lease.pdf"),
            &[
                "Oakridge lease: rent $1,900 per month.",
                "Term ends June 30, 2027.",
            ],
        );
        docx(
            &home.join("Documents/addendum.docx"),
            &[
                "Addendum to lease",
                "Pets allowed with a $300 deposit & approval.",
            ],
        );
        let before: Vec<_> = [
            "Documents/notes.md",
            "Documents/lease.pdf",
            "Documents/addendum.docx",
        ]
        .iter()
        .map(|p| fingerprint(&home.join(p)))
        .collect();

        let md = read(home, "Documents/notes.md", &[]).unwrap();
        assert_eq!(md.kind, "markdown");
        assert_eq!(
            md.text,
            "# Lease\n\nRent is $1,900 a month.\n\nEnds June 30, 2027."
        );
        let pdf = read(
            home,
            &home.join("Documents/lease.pdf").to_string_lossy(),
            &[],
        )
        .unwrap();
        assert_eq!(pdf.kind, "pdf");
        assert_eq!(pdf.pages, Some(2));
        assert!(pdf.text.contains("rent $1,900 per month"), "{}", pdf.text);
        assert!(pdf.text.contains("June 30, 2027"));
        let docx = read(home, "Documents/addendum.docx", &[]).unwrap();
        assert_eq!(docx.kind, "docx");
        assert_eq!(
            docx.text,
            "Addendum to lease\nPets allowed with a $300 deposit & approval."
        );

        let after: Vec<_> = [
            "Documents/notes.md",
            "Documents/lease.pdf",
            "Documents/addendum.docx",
        ]
        .iter()
        .map(|p| fingerprint(&home.join(p)))
        .collect();
        assert_eq!(before, after, "files are byte-for-byte unchanged");
    }

    #[test]
    fn refuses_outside_home_credentials_and_unreadable_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join("Pictures")).unwrap();
        std::fs::write(home.join(".ssh/id_ed25519"), "secret").unwrap();
        std::fs::write(home.join("Pictures/cat.jpg"), [0xff, 0xd8]).unwrap();
        std::fs::write(home.join("Pictures/.env"), "KEY=1").unwrap();
        std::fs::write(dir.path().join("outside.txt"), "hello").unwrap();
        assert!(read(&home, ".ssh/id_ed25519", &[])
            .unwrap_err()
            .contains("never reads"));
        assert!(read(&home, "Pictures/.env", &[])
            .unwrap_err()
            .contains("never reads"));
        assert!(read(
            &home,
            &dir.path().join("outside.txt").to_string_lossy(),
            &[]
        )
        .unwrap_err()
        .contains("home folder"));
        assert!(read(&home, "Pictures/cat.jpg", &[])
            .unwrap_err()
            .contains("not readable"));
        assert!(read(&home, "Pictures/missing.txt", &[])
            .unwrap_err()
            .contains("no file"));
        assert!(read(&home, "Pictures", &[]).is_err());
    }

    #[test]
    fn a_two_hundred_page_pdf_reads_quickly_and_comes_in_parts() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let pages: Vec<String> = (1..=200)
            .map(|i| format!("Page {i}. The quick brown fox jumps over the lazy dog, again and again, on page {i} of the long report."))
            .collect();
        let refs: Vec<&str> = pages.iter().map(String::as_str).collect();
        pdf(&home.join("report.pdf"), &refs);
        let started = std::time::Instant::now();
        let doc = read(home, "report.pdf", &[]).unwrap();
        let took = started.elapsed();
        eprintln!(
            "200 pages read in {took:?}: {} chars, {} parts",
            doc.text.chars().count(),
            doc.parts()
        );
        assert!(took < std::time::Duration::from_secs(30), "{took:?}");
        assert_eq!(doc.pages, Some(200));
        assert!(doc.parts() >= 3);
        let first = doc.part(1).unwrap();
        assert!(first.starts_with("Page 1."));
        assert!(doc.part(doc.parts()).unwrap().contains("page 200"));
        assert!(doc.part(doc.parts() + 1).is_none());
        assert!(doc.part(0).is_none());
    }
}
