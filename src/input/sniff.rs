//! Decides whether input is Mermaid source or Markdown.

use super::Format;

/// Looks at the first meaningful line, skipping blank lines, `%%` comments and a leading `---`
/// frontmatter block (Markdown documents often start with frontmatter too). Returns `None`
/// until that line has arrived; `complete` says that no more input will follow.
pub fn sniff(text: &str, complete: bool) -> Option<Format> {
    let mut lines = text.lines().map(str::trim);
    let mut at_start = true;
    while let Some(line) = lines.next() {
        if line.is_empty() || (line.starts_with("%%") && !line.starts_with("%%{")) {
            continue;
        }
        if line == "---" && at_start {
            at_start = false;
            if !lines.by_ref().any(|line| line == "---") {
                // An unclosed `---` at the end of the input is a Markdown thematic break.
                return complete.then_some(Format::Markdown);
            }
            continue;
        }
        return Some(if line.starts_with("%%{") || is_mermaid_header(line) {
            Format::Mermaid
        } else {
            Format::Markdown
        });
    }
    None
}

/// Headers distinctive enough to recognize by prefix, the way Mermaid does.
const DISTINCTIVE: &[&str] = &[
    "sequenceDiagram",
    "classDiagram",
    "stateDiagram",
    "erDiagram",
    "requirementDiagram",
    "quadrantChart",
    "gitGraph",
    "C4Context",
    "C4Container",
    "C4Component",
    "C4Dynamic",
    "C4Deployment",
    "flowchart-elk",
    "xychart",
    "sankey",
    "zenuml",
    "eventmodeling",
    "architecture-beta",
    "block-beta",
    "packet-beta",
    "radar-beta",
    "treemap-beta",
    "treeView-beta",
    "venn-beta",
    "swimlane-beta",
    "ishikawa-beta",
    "railroad-",
    "wardley-beta",
    "cynefin-beta",
];

/// Whether a line opens a Mermaid diagram. Headers that are ordinary words must stand alone
/// (apart from the options they take), so prose such as "pie charts are…" is not a diagram.
fn is_mermaid_header(line: &str) -> bool {
    if DISTINCTIVE.iter().any(|header| line.starts_with(header)) {
        return true;
    }
    let (word, rest) = line
        .split_once(|c: char| c.is_whitespace() || c == ';')
        .map_or((line, ""), |(word, rest)| (word, rest.trim()));
    match word {
        "graph" | "flowchart" => {
            let direction = rest.split(|c: char| c.is_whitespace() || c == ';').next();
            rest.is_empty() || matches!(direction, Some("TB" | "TD" | "BT" | "RL" | "LR"))
        }
        "pie" => rest.is_empty() || rest.starts_with("showData") || rest.starts_with("title "),
        "gantt" | "journey" | "timeline" | "mindmap" | "kanban" | "info" | "block" | "packet"
        | "architecture" | "treemap" | "ishikawa" => rest.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete(text: &str) -> Option<Format> {
        sniff(text, true)
    }

    #[test]
    fn mermaid_sources() {
        for text in [
            "flowchart LR\n  A --> B\n",
            "graph TD; A-->B",
            "graph\n A-->B",
            "  sequenceDiagram\n",
            "stateDiagram-v2\n",
            "pie\n",
            "pie title Pets\n",
            "timeline\n  title History\n",
            "architecture-beta\n",
            "C4Context\n",
            "gitGraph:\n",
            "%% a comment\n%%{init: {\"theme\": \"dark\"}}%%\ngraph LR\n",
            "\n\n---\ntitle: Flow\nconfig:\n  theme: dark\n---\nflowchart TB\n",
        ] {
            assert_eq!(complete(text), Some(Format::Mermaid), "{text:?}");
        }
    }

    #[test]
    fn markdown_documents() {
        for text in [
            "# Architecture\n\n```mermaid\ngraph TD\n```\n",
            "Here is the diagram you asked for:\n\n```mermaid\n",
            "---\ntitle: Blog post\ndate: 2026-09-11\n---\n\nSome prose.\n",
            "pie charts are underrated\n",
            "timeline of the project\n",
            "architecture overview\n",
            "graph theory basics\n",
            "- a list\n",
        ] {
            assert_eq!(complete(text), Some(Format::Markdown), "{text:?}");
        }
    }

    #[test]
    fn undecided_until_a_meaningful_line_arrives() {
        assert_eq!(complete(""), None);
        assert_eq!(complete("\n  \n%% just a comment\n"), None);
        assert_eq!(sniff("---\ntitle: x\n", false), None);
        assert_eq!(sniff("---\ntitle: x\n---\n\n", false), None);
        assert_eq!(sniff("---\ntitle: x\n---\nflowchart LR\n", false), Some(Format::Mermaid));
    }

    #[test]
    fn unclosed_frontmatter_at_the_end_is_markdown() {
        assert_eq!(complete("---\nnot frontmatter after all\n"), Some(Format::Markdown));
    }
}
