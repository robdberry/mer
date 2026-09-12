//! Converting Mermaid's HTML labels to SVG text in batches.
//!
//! Most diagram types put their labels in HTML inside `<foreignObject>`, which resvg can't draw,
//! and merman's resvg-safe pipeline converts them to SVG text. The conversion matches CSS for
//! every label within a fixed work budget that about a thousand labels use up, so an ER diagram
//! of 30 tables fails. This postprocessor runs first and gives merman's converter a batch of
//! labels at a time, in a document holding only those labels, their ancestors and the
//! stylesheets, which is everything the conversion reads. The text comes back in document order,
//! the labels are removed, and merman's own pass finds nothing left to convert.

use std::borrow::Cow;
use std::ops::Range;

use merman::svg::{
    ForeignObjectFallbackPostprocessor, RenderError, RenderResult, SvgPostprocessContext,
    SvgPostprocessor,
};

/// Labels per batch, well within the conversion's budget for typical labels.
pub const BATCH: usize = 256;

/// merman's name for the conversion's CSS matching budget.
const MATCH_BUDGET: &str = "svg_fallback_selector_index";

pub struct Batched {
    size: usize,
}

impl Batched {
    pub fn new(size: usize) -> Batched {
        Batched { size: size.max(1) }
    }
}

impl SvgPostprocessor for Batched {
    fn name(&self) -> &'static str {
        "mer-label-batches"
    }

    fn process<'a>(
        &self,
        svg: Cow<'a, str>,
        ctx: &SvgPostprocessContext<'_>,
    ) -> RenderResult<Cow<'a, str>> {
        // Labels inside `<switch>` are paired with the native text beside them, which a batch
        // leaves out, so those documents are left to merman.
        if svg.contains("<switch") {
            return Ok(svg);
        }
        let Some(document) = Document::scan(&svg).filter(|document| !document.labels.is_empty())
        else {
            return Ok(svg);
        };
        let mut text = String::new();
        let (mut start, mut size) = (0, self.size);
        while start < document.labels.len() {
            let end = document.labels.len().min(start + size);
            let batch = document.batch(&svg, start..end);
            match ForeignObjectFallbackPostprocessor.process(Cow::Borrowed(batch.as_str()), ctx) {
                Ok(converted) => match added(&batch, &converted) {
                    Some(added) => text.push_str(added),
                    None => return Ok(svg),
                },
                // Labels with unusually expensive styles get smaller batches.
                Err(RenderError::ResourceLimitExceeded(limit))
                    if limit.limit == MATCH_BUDGET && end - start > 1 =>
                {
                    size = (end - start) / 2;
                    continue;
                }
                Err(err) => return Err(err),
            }
            start = end;
        }
        Ok(Cow::Owned(document.without_labels(&svg, &text)))
    }
}

/// What merman's converter added to `batch`, which it otherwise returns unchanged.
fn added<'a>(batch: &str, converted: &'a str) -> Option<&'a str> {
    converted.strip_prefix(batch.strip_suffix("</svg>")?)?.strip_suffix("</svg>")
}

/// Where a document's labels, their ancestors and its stylesheets are.
struct Document {
    /// Start tags of the root and of every element with content, in document order.
    elements: Vec<Element>,
    styles: Vec<Range<usize>>,
    labels: Vec<Label>,
    /// Where the root element's end tag starts.
    end: usize,
}

struct Element {
    tag: Range<usize>,
    name: Range<usize>,
}

struct Label {
    span: Range<usize>,
    /// Ancestors below the root, outermost first, as indexes into `Document::elements`.
    ancestors: Vec<usize>,
}

impl Document {
    /// Reads SVG that merman has already validated as well-formed. `None` means the document has
    /// something this simple reader doesn't follow, and it is best left to merman.
    fn scan(svg: &str) -> Option<Document> {
        let mut elements: Vec<Element> = Vec::new();
        let mut open: Vec<usize> = Vec::new();
        let mut styles = Vec::new();
        let mut labels = Vec::new();
        let mut end = None;
        let mut at = 0;
        while let Some(found) = svg[at..].find('<') {
            let start = at + found;
            let rest = &svg[start..];
            at = if rest.starts_with("<!--") {
                start + rest.find("-->")? + "-->".len()
            } else if rest.starts_with("<![CDATA[") {
                start + rest.find("]]>")? + "]]>".len()
            } else if rest.starts_with("<!") || rest.starts_with("<?") {
                start + rest.find('>')? + 1
            } else if let Some(name) = rest.strip_prefix("</") {
                let element = &elements[open.pop()?];
                if name[..name.find('>')?].trim_end() != &svg[element.name.clone()] {
                    return None;
                }
                if open.is_empty() {
                    end = Some(start);
                }
                start + rest.find('>')? + 1
            } else {
                let close = tag_end(svg, start)?;
                let length = rest[1..].find(|c: char| c.is_ascii_whitespace() || matches!(c, '/' | '>'))?;
                let name = start + 1..start + 1 + length;
                let empty = svg[..close].ends_with("/>");
                let tag_name = &svg[name.clone()];
                let root = open.is_empty();
                if root && (end.is_some() || tag_name != "svg" || empty) {
                    return None;
                }
                // merman matches these names regardless of case; other spellings are its to handle.
                let spelled = |expected: &str| tag_name.eq_ignore_ascii_case(expected);
                if (spelled("foreignObject") && tag_name != "foreignObject")
                    || (spelled("style") && tag_name != "style")
                {
                    return None;
                }
                match tag_name {
                    "foreignObject" if empty => return None,
                    "foreignObject" => {
                        let after = close + svg[close..].find("</foreignObject>")? + "</foreignObject>".len();
                        labels.push(Label { span: start..after, ancestors: open[1..].to_vec() });
                        after
                    }
                    "style" if !empty => {
                        let after = close + svg[close..].find("</style>")? + "</style>".len();
                        styles.push(start..after);
                        after
                    }
                    _ if !empty => {
                        open.push(elements.len());
                        elements.push(Element { tag: start..close, name });
                        close
                    }
                    _ => close,
                }
            };
        }
        if !open.is_empty() {
            return None;
        }
        Some(Document { elements, styles, labels, end: end? })
    }

    /// A document with the labels in `range`, their ancestors and every stylesheet.
    fn batch(&self, svg: &str, range: Range<usize>) -> String {
        let mut out = String::new();
        out.push_str(&svg[self.elements[0].tag.clone()]);
        for style in &self.styles {
            out.push_str(&svg[style.clone()]);
        }
        let mut open: &[usize] = &[];
        for label in &self.labels[range] {
            let shared = open.iter().zip(&label.ancestors).take_while(|(a, b)| a == b).count();
            self.close(&mut out, svg, &open[shared..]);
            for &element in &label.ancestors[shared..] {
                out.push_str(&svg[self.elements[element].tag.clone()]);
            }
            out.push_str(&svg[label.span.clone()]);
            open = &label.ancestors;
        }
        self.close(&mut out, svg, open);
        out.push_str("</svg>");
        out
    }

    fn close(&self, out: &mut String, svg: &str, elements: &[usize]) {
        for &element in elements.iter().rev() {
            out.push_str("</");
            out.push_str(&svg[self.elements[element].name.clone()]);
            out.push('>');
        }
    }

    /// `svg` without its labels, and with `text` at the end of the root element.
    fn without_labels(&self, svg: &str, text: &str) -> String {
        let mut out = String::with_capacity(svg.len() + text.len());
        let mut at = 0;
        for label in &self.labels {
            out.push_str(&svg[at..label.span.start]);
            at = label.span.end;
        }
        out.push_str(&svg[at..self.end]);
        out.push_str(text);
        out.push_str(&svg[self.end..]);
        out
    }
}

/// The position after the `>` that ends the tag starting at `start`, outside quoted values.
fn tag_end(svg: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, &byte) in svg.as_bytes()[start..].iter().enumerate() {
        match (quote, byte) {
            (Some(open), _) if open == byte => quote = None,
            (None, b'"' | b'\'') => quote = Some(byte),
            (None, b'>') => return Some(start + offset + 1),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVG: &str = concat!(
        r#"<svg id="m"><style>#m .a > p{color:red}</style><!-- <foreignObject> -->"#,
        r#"<g class="a"><g class="b"><foreignObject width="1" height="1"><div>1</div></foreignObject></g>"#,
        r#"<rect title="a > b"/><g class="c"><foreignObject width="1" height="1"><div>2</div></foreignObject></g></g>"#,
        r#"<g class="d"><foreignObject width="1" height="1"><div>3</div></foreignObject></g></svg>"#,
    );

    const HEAD: &str = r#"<svg id="m"><style>#m .a > p{color:red}</style>"#;

    fn label(text: &str) -> String {
        format!(r#"<foreignObject width="1" height="1"><div>{text}</div></foreignObject>"#)
    }

    #[test]
    fn batches_hold_labels_with_their_ancestors_and_stylesheets() {
        let document = Document::scan(SVG).unwrap();
        assert_eq!(document.labels.len(), 3);
        assert_eq!(
            document.batch(SVG, 0..3),
            format!(
                r#"{HEAD}<g class="a"><g class="b">{}</g><g class="c">{}</g></g><g class="d">{}</g></svg>"#,
                label("1"),
                label("2"),
                label("3")
            )
        );
        assert_eq!(
            document.batch(SVG, 1..2),
            format!(r#"{HEAD}<g class="a"><g class="c">{}</g></g></svg>"#, label("2"))
        );
    }

    #[test]
    fn labels_are_replaced_by_text_at_the_end() {
        let document = Document::scan(SVG).unwrap();
        assert_eq!(
            document.without_labels(SVG, "<text/>"),
            concat!(
                r#"<svg id="m"><style>#m .a > p{color:red}</style><!-- <foreignObject> -->"#,
                r#"<g class="a"><g class="b"></g><rect title="a > b"/><g class="c"></g></g>"#,
                r#"<g class="d"></g><text/></svg>"#,
            )
        );
    }

    #[test]
    fn unfamiliar_documents_are_left_alone() {
        assert!(Document::scan("<g><foreignObject></foreignObject></g>").is_none());
        assert!(Document::scan("<svg><g></svg>").is_none());
        assert!(Document::scan("<svg><foreignobject></foreignobject></svg>").is_none());
        assert!(Document::scan(r#"<svg><foreignObject width="1"/></svg>"#).is_none());
        assert!(Document::scan("<svg></svg><svg></svg>").is_none());
    }

    #[test]
    fn only_added_text_is_taken_from_the_conversion() {
        assert_eq!(added("<svg>x</svg>", "<svg>x<text/></svg>"), Some("<text/>"));
        assert_eq!(added("<svg>x</svg>", "<svg>x</svg>"), Some(""));
        assert_eq!(added("<svg>x</svg>", "<svg>y<text/></svg>"), None);
    }
}
