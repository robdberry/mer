//! Diagram sources: Mermaid files, Markdown fences, and stdin.

pub mod markdown;
pub mod sniff;

use std::path::Path;

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
        .flat_map(|document| diagrams(document, "<stdin>", format))
        .collect()
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
}
