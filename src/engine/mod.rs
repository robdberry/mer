//! The Mermaid engine. merman is used only in this module, behind a small interface.

mod measure;

use merman::ascii::{AsciiError, AsciiRenderOptions, AsciiViewportPolicy, OverflowPolicy};
use merman::svg::{RootBackgroundPostprocessor, SvgPipeline};
use merman::{
    AsciiRequest, MermaidConfig, ParseOptions, RenderError, RenderOutput, RenderRequest, Renderer,
    SvgEnvironment, SvgRequest,
};
use serde_json::Value;

use crate::diag::Diagnostic;

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

impl Engine {
    /// `config` is Mermaid site configuration; frontmatter and directives inside a diagram
    /// override it. `background` paints the SVG root; without one the root is transparent.
    pub fn new(config: Value, background: Option<&str>) -> Engine {
        let engine = merman::Engine::new().with_site_config(MermaidConfig::from_value(config));
        let renderer = Renderer::new()
            .with_engine(engine)
            .with_parse_options(ParseOptions::strict());
        // Mermaid gives the root a white background, which would cover the terminal's.
        let pipeline = SvgPipeline::resvg_safe().with_postprocessor(
            RootBackgroundPostprocessor::new(background.unwrap_or("transparent")),
        );
        let request = SvgRequest {
            environment: SvgEnvironment::deterministic()
                .with_text_measurement_policy(measure::policy()),
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
            viewport,
            ..AsciiRequest::default()
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
