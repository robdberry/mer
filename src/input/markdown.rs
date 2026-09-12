//! Extraction of `mermaid` fenced code blocks from Markdown.
//!
//! A line-based scanner rather than a full CommonMark parser, so the same code serves complete
//! files and streams that are still arriving.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fence {
    pub text: String,
    /// Zero-based line on which the fence content starts.
    pub first_line: usize,
    /// The nearest heading above the fence.
    pub caption: Option<String>,
}

#[derive(Debug, Default)]
pub struct Scanner {
    line: usize,
    heading: Option<String>,
    open: Option<Open>,
}

#[derive(Debug)]
struct Open {
    marker: u8,
    len: usize,
    indent: usize,
    mermaid: bool,
    fence: Fence,
}

impl Scanner {
    pub fn new() -> Scanner {
        Scanner::default()
    }

    /// Feeds one line without its line ending. Returns the `mermaid` fence this line closed.
    pub fn push_line(&mut self, line: &str) -> Option<Fence> {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let number = self.line;
        self.line += 1;

        if let Some(open) = self.open.as_mut() {
            if !is_closing(line, open.marker, open.len) {
                if open.mermaid {
                    open.fence.text.push_str(strip_indent(line, open.indent));
                    open.fence.text.push('\n');
                }
                return None;
            }
            let open = self.open.take()?;
            return open.mermaid.then_some(open.fence);
        }

        if let Some((marker, len, indent, info)) = opening(line) {
            self.open = Some(Open {
                marker,
                len,
                indent,
                mermaid: is_mermaid_info(info),
                fence: Fence {
                    text: String::new(),
                    first_line: number + 1,
                    caption: self.heading.clone(),
                },
            });
        } else if let Some(title) = heading(line) {
            self.heading = Some(title);
        }
        None
    }

    /// Ends the input. A `mermaid` fence that is still open runs to the end of the document.
    pub fn finish(self) -> Option<Fence> {
        self.open.filter(|open| open.mermaid).map(|open| open.fence)
    }
}

pub fn extract(text: &str) -> Vec<Fence> {
    let mut scanner = Scanner::new();
    let mut fences: Vec<Fence> = text.lines().filter_map(|line| scanner.push_line(line)).collect();
    fences.extend(scanner.finish());
    fences
}

fn indentation(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Recognizes an opening fence, returning its marker, length, indentation and info string.
fn opening(line: &str) -> Option<(u8, usize, usize, &str)> {
    let indent = indentation(line);
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let marker = *rest.as_bytes().first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let len = rest.bytes().take_while(|&b| b == marker).count();
    let info = rest[len..].trim();
    if len < 3 || (marker == b'`' && info.contains('`')) {
        return None;
    }
    Some((marker, len, indent, info))
}

fn is_closing(line: &str, marker: u8, len: usize) -> bool {
    let indent = indentation(line);
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let count = rest.bytes().take_while(|&b| b == marker).count();
    count >= len && rest[count..].trim().is_empty()
}

/// Accepts `mermaid` as the info string's first word, also in the `{mermaid}` form.
fn is_mermaid_info(info: &str) -> bool {
    let word = info
        .split(|c: char| c.is_whitespace() || c == ',')
        .next()
        .unwrap_or("");
    let word = word.strip_prefix('{').unwrap_or(word);
    let word = word.strip_suffix('}').unwrap_or(word);
    word.eq_ignore_ascii_case("mermaid")
}

fn strip_indent(line: &str, indent: usize) -> &str {
    &line[indentation(line).min(indent)..]
}

fn heading(line: &str) -> Option<String> {
    let indent = indentation(line);
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let level = rest.bytes().take_while(|&b| b == b'#').count();
    let after = &rest[level..];
    if level == 0 || level > 6 || !(after.is_empty() || after.starts_with([' ', '\t'])) {
        return None;
    }
    let title = after.trim();
    let without_closing = title.trim_end_matches('#');
    let title = if without_closing.is_empty() || without_closing.ends_with([' ', '\t']) {
        without_closing.trim_end()
    } else {
        title
    };
    (!title.is_empty()).then(|| title.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_mermaid_fences_with_positions() {
        let doc = "intro\n```mermaid\ngraph TD\n  A-->B\n```\ntext\n```rust\nfn main() {}\n```\n";
        assert_eq!(
            extract(doc),
            vec![Fence {
                text: "graph TD\n  A-->B\n".to_string(),
                first_line: 2,
                caption: None,
            }]
        );
    }

    #[test]
    fn captions_come_from_the_nearest_heading() {
        let doc = "# Top\n## Flow ##\n```mermaid\na\n```\n### C#\n~~~mermaid\nb\n~~~\n";
        let captions: Vec<_> = extract(doc).into_iter().map(|f| f.caption).collect();
        assert_eq!(captions, [Some("Flow".to_string()), Some("C#".to_string())]);
    }

    #[test]
    fn headings_inside_fences_are_ignored() {
        let doc = "# Real\n```sh\n# comment\n```\n```mermaid\nx\n```\n";
        assert_eq!(extract(doc)[0].caption.as_deref(), Some("Real"));
    }

    #[test]
    fn longer_fences_can_contain_shorter_ones() {
        let doc = "````markdown\n```mermaid\nnot a diagram\n```\n````\n````mermaid\ngraph\n```\nstill inside\n````\n";
        let fences = extract(doc);
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].text, "graph\n```\nstill inside\n");
    }

    #[test]
    fn indentation_is_removed_up_to_the_fence_indent() {
        let doc = "  ```mermaid\n    graph\n   A\n B\n  ```\n";
        assert_eq!(extract(doc)[0].text, "  graph\n A\nB\n");
    }

    #[test]
    fn info_string_variants() {
        assert!(is_mermaid_info("mermaid"));
        assert!(is_mermaid_info("Mermaid title=x"));
        assert!(is_mermaid_info("{mermaid}"));
        assert!(!is_mermaid_info("mermaidjs"));
        assert!(!is_mermaid_info(""));
        assert!(opening("```mermaid").is_some());
        assert!(opening("``` mermaid ` x").is_none());
        assert!(opening("    ```mermaid").is_none());
        assert!(opening("``mermaid").is_none());
    }

    #[test]
    fn unclosed_fences_run_to_the_end() {
        let fences = extract("```mermaid\ngraph TD\nA-->B");
        assert_eq!(fences[0].text, "graph TD\nA-->B\n");
    }

    #[test]
    fn streaming_reports_fences_as_they_close() {
        let mut scanner = Scanner::new();
        assert_eq!(scanner.push_line("Here you go:"), None);
        assert_eq!(scanner.push_line("```mermaid"), None);
        assert_eq!(scanner.push_line("pie"), None);
        let fence = scanner.push_line("```\r").expect("closed fence");
        assert_eq!(fence.text, "pie\n");
        assert_eq!(scanner.finish(), None);
    }
}
