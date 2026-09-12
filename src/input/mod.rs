//! Diagram sources: Mermaid files, Markdown fences, and stdin.

pub mod markdown;
pub mod sniff;

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The origin of diagrams read from stdin.
pub const STDIN: &str = "<stdin>";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Mermaid,
    Markdown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagram {
    pub text: String,
    /// Where the diagram came from, for messages: a path or `<stdin>`.
    pub origin: String,
    /// Zero-based line of the input on which `text` starts.
    pub first_line: usize,
    /// The nearest Markdown heading above the diagram.
    pub caption: Option<String>,
}

pub fn format_from_extension(path: &Path) -> Option<Format> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "mmd" | "mermaid" => Some(Format::Mermaid),
        "md" | "markdown" | "mdx" => Some(Format::Markdown),
        _ => None,
    }
}

/// Splits one complete document into diagrams. Pass `None` to detect the format.
pub fn diagrams(text: &str, origin: &str, format: Option<Format>) -> Vec<Diagram> {
    match format.or_else(|| sniff::sniff(text, true)) {
        None => Vec::new(),
        Some(Format::Mermaid) => vec![Diagram {
            text: text.to_string(),
            origin: origin.to_string(),
            first_line: 0,
            caption: None,
        }],
        Some(Format::Markdown) => markdown::extract(text)
            .into_iter()
            .map(|fence| Diagram {
                text: fence.text,
                origin: origin.to_string(),
                first_line: fence.first_line,
                caption: fence.caption,
            })
            .collect(),
    }
}

/// Splits stdin into its NUL-separated documents and returns their diagrams in order.
pub fn documents(text: &str, format: Option<Format>) -> Vec<Diagram> {
    text.split('\0')
        .filter(|document| !document.trim().is_empty())
        .flat_map(|document| diagrams(document, STDIN, format))
        .collect()
}

/// Reads every input to the end: files by their extension, and stdin for `-`. With `selected`,
/// only that diagram of each input is kept, counting from 1.
pub fn read(
    inputs: &[PathBuf],
    stdin_format: Option<Format>,
    selected: Option<usize>,
) -> Result<Vec<Diagram>> {
    let mut all = Vec::new();
    let mut read_stdin = false;
    for path in inputs {
        let (origin, found) = if path.as_os_str() == "-" {
            if std::mem::replace(&mut read_stdin, true) {
                bail!("stdin can only be read once");
            }
            let mut bytes = Vec::new();
            io::stdin()
                .lock()
                .read_to_end(&mut bytes)
                .context("cannot read stdin")?;
            let text = String::from_utf8_lossy(&bytes);
            (STDIN.to_string(), documents(&text, stdin_format))
        } else {
            let text = fs::read_to_string(path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            let origin = path.display().to_string();
            let found = diagrams(&text, &origin, format_from_extension(path));
            (origin, found)
        };
        match selected {
            None => all.extend(found),
            Some(n) => {
                let count = found.len();
                let diagram = found.into_iter().nth(n.saturating_sub(1));
                all.push(diagram.with_context(|| {
                    format!("{origin} has {count} diagram(s), so there is no diagram {n}")
                })?);
            }
        }
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nul_separated_documents_follow_each_other() {
        let found = documents("graph TD\n A-->B\n\0\0pie\n\0", None);
        let texts: Vec<_> = found.iter().map(|d| d.text.as_str()).collect();
        assert_eq!(texts, ["graph TD\n A-->B\n", "pie\n"]);
    }

    #[test]
    fn formats_from_extensions() {
        assert_eq!(format_from_extension(Path::new("a.mmd")), Some(Format::Mermaid));
        assert_eq!(format_from_extension(Path::new("a.Mermaid")), Some(Format::Mermaid));
        assert_eq!(format_from_extension(Path::new("docs/README.md")), Some(Format::Markdown));
        assert_eq!(format_from_extension(Path::new("a.txt")), None);
        assert_eq!(format_from_extension(Path::new("Makefile")), None);
    }

    #[test]
    fn markdown_documents_yield_their_fences() {
        let doc = "# Title\n\n```mermaid\ngraph TD\n  A-->B\n```\n\n## Next\n```mermaid\npie\n```\n";
        let found = diagrams(doc, "doc.md", None);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].text, "graph TD\n  A-->B\n");
        assert_eq!(found[0].first_line, 3);
        assert_eq!(found[0].caption.as_deref(), Some("Title"));
        assert_eq!(found[1].caption.as_deref(), Some("Next"));
        assert_eq!(found[1].origin, "doc.md");
    }

    #[test]
    fn mermaid_documents_are_one_diagram() {
        let found = diagrams("flowchart LR\n  A --> B\n", "<stdin>", None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].first_line, 0);
        assert!(diagrams("\n\n", "<stdin>", None).is_empty());
    }

    #[test]
    fn selecting_keeps_that_diagram_of_each_input() {
        let doc = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/doc.md"));
        let inputs = [doc.clone(), doc];
        assert_eq!(read(&inputs[..1], None, None).unwrap().len(), 2);
        let selected = read(&inputs, None, Some(2)).unwrap();
        let captions: Vec<_> = selected.iter().map(|d| d.caption.as_deref()).collect();
        assert_eq!(captions, [Some("States"), Some("States")]);
        let missing = read(&inputs[..1], None, Some(3)).unwrap_err().to_string();
        assert!(missing.ends_with("has 2 diagram(s), so there is no diagram 3"), "{missing}");
    }
}
