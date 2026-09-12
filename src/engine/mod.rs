//! The Mermaid engine. merman is used only in this module, behind a small interface.

mod labels;
mod measure;

use merman::ascii::{
    AsciiError, AsciiRenderOptions, AsciiResourcePolicy, AsciiViewportPolicy, OverflowPolicy,
};
use merman::resources::{InputResourcePolicy, ResourceProfile};
use merman::svg::{RenderResourcePolicy, RootBackgroundPostprocessor, SvgPipeline};
use merman::{
    AsciiRequest, MermaidConfig, ParseOptions, RenderError, RenderOutput, RenderRequest, Renderer,
    SvgEnvironment, SvgRequest,
};
use serde_json::Value;

use crate::diag::{Diagnostic, NO_DIAGRAM};

pub use merman::OperationControl as Control;

pub struct Engine {
    renderer: Renderer,
    request: SvgRequest,
}

#[derive(Clone, Debug)]
pub enum Failure {
    /// The input holds no diagram.
    Empty,
    Diagnostic(Diagnostic),
    Cancelled,
}

impl Failure {
    /// What to tell the user. A cancelled render isn't an error and has nothing to say.
    pub fn diagnostic(&self) -> Option<Diagnostic> {
        match self {
            Failure::Empty => Some(Diagnostic {
                message: NO_DIAGRAM.to_string(),
                span: None,
            }),
            Failure::Diagnostic(diagnostic) => Some(diagnostic.clone()),
            Failure::Cancelled => None,
        }
    }
}

impl Engine {
    /// `config` is Mermaid site configuration; frontmatter and directives inside a diagram
    /// override it. `background` paints the SVG root; without one the root is transparent.
    pub fn new(config: Value, background: Option<&str>) -> Engine {
        Engine::with_label_batches(config, background, Some(labels::BATCH))
    }

    /// Like `new`, with HTML labels converted to SVG text `batch` labels at a time, or all at
    /// once by merman without a batch size.
    fn with_label_batches(config: Value, background: Option<&str>, batch: Option<usize>) -> Engine {
        let engine = merman::Engine::new().with_site_config(MermaidConfig::from_value(config));
        // merman's default budgets are meant for untrusted input and reject large diagrams, such
        // as schemas with a few hundred tables. mer draws the user's own files, and merman's hard
        // limits still apply.
        let renderer = Renderer::new()
            .with_engine(engine)
            .with_parse_options(ParseOptions::strict())
            .with_resource_policy(InputResourcePolicy::for_profile(
                ResourceProfile::UnboundedForTrustedInput,
            ));
        let mut pipeline = SvgPipeline::resvg_safe();
        if let Some(size) = batch {
            pipeline = pipeline.with_postprocessor(labels::Batched::new(size));
        }
        // Mermaid gives the root a white background, which would cover the terminal's.
        let pipeline = pipeline.with_postprocessor(RootBackgroundPostprocessor::new(
            background.unwrap_or("transparent"),
        ));
        let request = SvgRequest {
            environment: SvgEnvironment::deterministic()
                .with_text_measurement_policy(measure::policy())
                .with_resource_policy(RenderResourcePolicy::unbounded_for_trusted_input()),
            pipeline: Some(pipeline),
            ..SvgRequest::default()
        };
        Engine { renderer, request }
    }

    /// Renders one diagram to SVG that resvg can draw completely.
    pub fn render_svg(&self, source: &str, control: Control) -> Result<String, Failure> {
        let request = RenderRequest::svg(source, control, self.request.clone());
        match self.renderer.render(request) {
            Ok(RenderOutput::Svg(Some(output))) => Ok(output.into_parts().0),
            Ok(_) => Err(Failure::Empty),
            Err(err) => Err(failure(err)),
        }
    }

    /// Draws one diagram with Unicode box-drawing characters. A drawing wider than `max_width`
    /// columns is an error, because merman's fallback for drawings that don't fit lists its
    /// internal model instead.
    pub fn render_text(&self, source: &str, max_width: Option<usize>) -> Result<String, Failure> {
        let viewport = match max_width {
            Some(width) => AsciiViewportPolicy::with_max_width(width).overflow(OverflowPolicy::Error),
            None => AsciiViewportPolicy::unrestricted(),
        };
        let request = AsciiRequest {
            options: AsciiRenderOptions::unicode(),
            resources: AsciiResourcePolicy::unbounded(),
            viewport,
        };
        match self.renderer.render(RenderRequest::ascii(source, Control::new(), request)) {
            Ok(RenderOutput::Ascii(Some(output))) => Ok(output.text),
            Ok(_) => Err(Failure::Empty),
            Err(RenderError::Ascii(AsciiError::WidthOverflow { max_width, actual_width, .. })) => {
                Err(Failure::Diagnostic(Diagnostic {
                    message: format!(
                        "the diagram is {actual_width} columns wide but the terminal has \
                         {max_width}; widen the terminal or write an image with -o FILE"
                    ),
                    span: None,
                }))
            }
            Err(RenderError::Ascii(err)) => Err(Failure::Diagnostic(Diagnostic {
                message: format!("{err}; write an image with -o FILE instead"),
                span: None,
            })),
            Err(err) => Err(failure(err)),
        }
    }
}

fn failure(err: RenderError) -> Failure {
    match err {
        RenderError::NoDiagram => Failure::Empty,
        RenderError::Cancelled(_) => Failure::Cancelled,
        RenderError::Parse(diagnostic) => Failure::Diagnostic(Diagnostic {
            message: without_prefix(&diagnostic.terminal_safe_message()),
            span: diagnostic
                .terminal_diagnostic_details()
                .span
                .map(|span| (span.start, span.end)),
        }),
        other => Failure::Diagnostic(Diagnostic {
            message: other.to_string(),
            span: None,
        }),
    }
}

/// Drops merman's `Diagram parse error (flowchart-v2): ` prefix; the code frame shows where.
fn without_prefix(message: &str) -> String {
    message
        .strip_prefix("Diagram parse error (")
        .and_then(|rest| rest.split_once("): "))
        .map_or(message, |(_, rest)| rest)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        Engine::new(serde_json::json!({ "fontFamily": crate::fonts::FAMILY }), None)
    }

    #[test]
    fn renders_resvg_safe_svg() {
        let svg = engine()
            .render_svg("flowchart LR\n  A[Start] --> B[Done]\n", Control::new())
            .unwrap();
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("Start") && svg.contains("Done"));
        assert!(!svg.contains("foreignObject"));
        assert!(!svg.contains("background-color:white"));
    }

    #[test]
    fn label_batches_draw_what_merman_draws() {
        let config = crate::display::site_config("default", None, None);
        let whole = Engine::with_label_batches(config.clone(), None, None);
        let batched = Engine::with_label_batches(config, None, Some(2));
        let gallery = include_str!("../../tests/fixtures/gallery.md");
        let diagrams =
            crate::input::diagrams(gallery, "gallery.md", Some(crate::input::Format::Markdown));
        assert!(diagrams.len() >= 20);
        for diagram in &diagrams {
            let expected = whole.render_svg(&diagram.text, Control::new()).ok();
            let actual = batched.render_svg(&diagram.text, Control::new()).ok();
            assert!(expected.is_some(), "{:?} does not render", diagram.caption);
            assert!(expected == actual, "{:?} differs when drawn in batches", diagram.caption);
        }
    }

    #[test]
    fn large_schemas_render() {
        // More layout work than merman's default budget allows, and more labels than its label
        // conversion handles in one pass.
        let mut source = String::from("erDiagram\n");
        for table in 0..250 {
            source.push_str(&format!("  TABLE_{table} {{\n"));
            for column in 0..8 {
                source.push_str(&format!("    string column_{column}\n"));
            }
            source.push_str("  }\n");
            if table > 0 {
                source.push_str(&format!("  TABLE_{} ||--o{{ TABLE_{table} : has\n", table / 2));
            }
        }
        for link in 0..125 {
            let (a, b) = (link * 7 % 250, (link * 13 + 5) % 250);
            source.push_str(&format!("  TABLE_{a} }}o--o{{ TABLE_{b} : links\n"));
        }
        let svg = engine().render_svg(&source, Control::new()).unwrap();
        assert!(svg.contains(">TABLE_249</text>") && !svg.contains("<foreignObject"));
    }

    #[test]
    fn parse_errors_carry_a_span() {
        let source = "flowchart TD\n  A[Start] --> B{Choice}\n  B -->|yes| C[Done\n";
        let Err(Failure::Diagnostic(diagnostic)) = engine().render_svg(source, Control::new())
        else {
            panic!("expected a diagnostic");
        };
        assert_eq!(diagnostic.message, "Unterminated node label (missing `]`)");
        let (start, _) = diagnostic.span.expect("span");
        assert_eq!(&source[start..start + 1], "[");
    }

    #[test]
    fn empty_input_is_not_a_diagram() {
        assert!(matches!(
            engine().render_svg("", Control::new()),
            Err(Failure::Empty | Failure::Diagnostic(_))
        ));
    }

    #[test]
    fn cancelled_renders_stop() {
        let control = Control::new();
        control.cancel();
        assert!(matches!(
            engine().render_svg("flowchart LR\n A --> B\n", control),
            Err(Failure::Cancelled)
        ));
    }

    #[test]
    fn draws_diagrams_as_text() {
        let text = engine()
            .render_text("flowchart LR\n  A[Start] --> B[Done]\n", Some(80))
            .unwrap();
        assert!(text.contains("Start") && text.contains("Done"), "{text}");
        assert!(text.chars().any(|c| ('\u{2500}'..='\u{257f}').contains(&c)), "{text}");
    }

    #[test]
    fn text_wider_than_the_terminal_is_an_error() {
        let source = "flowchart LR\n  A[Read the input] --> B[Parse the diagram] --> C[Send it]\n";
        let Err(Failure::Diagnostic(diagnostic)) = engine().render_text(source, Some(40)) else {
            panic!("the drawing should not fit in 40 columns");
        };
        assert!(diagnostic.message.contains("but the terminal has 40;"), "{}", diagnostic.message);
        let text = engine().render_text(source, None).unwrap();
        assert!(text.lines().any(|line| line.chars().count() > 40), "{text}");
    }

    #[test]
    fn text_output_names_what_it_cannot_draw() {
        let Err(Failure::Diagnostic(diagnostic)) =
            engine().render_text("pie\n  \"Dogs\" : 3\n", Some(80))
        else {
            panic!("pie charts have no text rendering");
        };
        assert!(diagnostic.message.contains("-o FILE"), "{}", diagnostic.message);
    }

    #[test]
    fn prefix_is_removed_only_when_present() {
        assert_eq!(without_prefix("Diagram parse error (pie): bad"), "bad");
        assert_eq!(without_prefix("No Mermaid diagram type detected"), "No Mermaid diagram type detected");
    }
}
