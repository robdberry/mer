//! Diagnostics printed with an excerpt of the source they point into.

use crate::input::Diagram;

/// What mer says about input that holds no diagram.
pub const NO_DIAGRAM: &str = "no Mermaid diagram found";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub message: String,
    /// Byte range within the diagram text.
    pub span: Option<(usize, usize)>,
}

/// Formats a diagnostic with a code frame whose line numbers refer to the original input.
pub fn format(diagnostic: &Diagnostic, diagram: &Diagram, color: bool) -> String {
    let (red, blue, bold, reset) = if color {
        ("\x1b[1;31m", "\x1b[1;34m", "\x1b[1m", "\x1b[0m")
    } else {
        ("", "", "", "")
    };
    let mut out = format!("{red}error{reset}{bold}: {}{reset}\n", diagnostic.message);
    let Some((start, _)) = diagnostic.span else {
        out.push_str(&format!(" {blue}-->{reset} {}\n", diagram.origin));
        return out;
    };

    let text = diagram.text.as_str();
    let mut start = start.min(text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[start..].find('\n').map_or(text.len(), |i| start + i);
    let source = text[line_start..line_end].trim_end_matches('\r');
    let column = text[line_start..start].chars().count() + 1;
    let number = diagram.first_line + text[..start].matches('\n').count() + 1;

    let pad = " ".repeat(number.to_string().len());
    let marker: String = source
        .chars()
        .take(column - 1)
        .map(|c| if c == '\t' { '\t' } else { ' ' })
        .collect();
    out.push_str(&format!(
        "{pad}{blue}-->{reset} {}:{number}:{column}\n",
        diagram.origin
    ));
    out.push_str(&format!("{pad} {blue}|{reset}\n"));
    out.push_str(&format!("{blue}{number} |{reset} {source}\n"));
    out.push_str(&format!("{pad} {blue}|{reset} {marker}{red}^{reset}\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagram(text: &str, first_line: usize) -> Diagram {
        Diagram {
            text: text.to_string(),
            origin: "docs/arch.md".to_string(),
            first_line,
            caption: None,
        }
    }

    #[test]
    fn points_at_the_file_line_and_column() {
        let d = diagram("flowchart TD\n  A[Start] --> B{Choice}\n  B -->|yes| C[Done\n", 41);
        let diagnostic = Diagnostic {
            message: "Unterminated node label (missing `]`)".to_string(),
            span: Some((52, 53)),
        };
        assert_eq!(
            format(&diagnostic, &d, false),
            "error: Unterminated node label (missing `]`)\n\
             \x20 --> docs/arch.md:44:15\n\
             \x20  |\n\
             44 |   B -->|yes| C[Done\n\
             \x20  |               ^\n"
        );
    }

    #[test]
    fn diagnostics_without_a_span_name_the_origin() {
        let diagnostic = Diagnostic {
            message: "No Mermaid diagram type detected".to_string(),
            span: None,
        };
        assert_eq!(
            format(&diagnostic, &diagram("hello", 0), false),
            "error: No Mermaid diagram type detected\n --> docs/arch.md\n"
        );
    }

    #[test]
    fn spans_past_the_end_or_inside_characters_are_clamped() {
        let d = diagram("graph\n  é", 0);
        let out = format(
            &Diagnostic {
                message: "x".to_string(),
                span: Some((9, 10)),
            },
            &d,
            false,
        );
        assert!(out.contains("docs/arch.md:2:3"), "{out}");
        let out = format(
            &Diagnostic {
                message: "x".to_string(),
                span: Some((100, 100)),
            },
            &d,
            false,
        );
        assert!(out.contains("docs/arch.md:2:4"), "{out}");
    }

    #[test]
    fn tabs_are_kept_so_the_caret_lines_up() {
        let d = diagram("\tA --> [", 0);
        let out = format(
            &Diagnostic {
                message: "x".to_string(),
                span: Some((7, 8)),
            },
            &d,
            false,
        );
        assert!(out.ends_with("| \t      ^\n"), "{out:?}");
    }
}
