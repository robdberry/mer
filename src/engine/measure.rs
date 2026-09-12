//! Label measurement with the embedded Inter font.
//!
//! merman's deterministic measurer reproduces Mermaid's browser measurements in detail
//! (wrapping, glyph overhangs, line heights), but its widths assume Mermaid's default fonts and
//! Inter is wider. This measurer keeps every one of those operations and corrects only the
//! widths, scaling each by the ratio of Inter's shaped width to the deterministic width of the
//! same text.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use harfrust::{FontRef, ShapeOptions, ShaperData, UnicodeBuffer};
use merman::svg::{
    DeterministicTextMeasurer, MeasurementProfileId, TextMeasurementPolicy,
    TextMeasurementProfile, TextMeasurementProfileIdentity, TextMeasurer, TextMetrics, TextStyle,
    WrapMode,
};

use crate::fonts::{self, FACES};

pub fn policy() -> TextMeasurementPolicy {
    let profile = MeasurementProfileId::new("mer.inter").expect("profile id is not empty");
    let identity = TextMeasurementProfileIdentity::new(profile, env!("CARGO_PKG_VERSION"))
        .expect("version is not empty");
    TextMeasurementPolicy::uniform(TextMeasurementProfile::new(
        identity,
        Arc::new(InterMeasurer::new()),
    ))
}

pub struct InterMeasurer {
    base: DeterministicTextMeasurer,
    state: Mutex<State>,
}

struct State {
    shapers: Vec<(FontRef<'static>, ShaperData)>,
    ratios: HashMap<Key, f64>,
}

#[derive(PartialEq, Eq, Hash)]
struct Key {
    text: String,
    size: u64,
    face: usize,
}

/// Cached ratios kept before the cache is cleared, which bounds memory in long sessions.
const MAX_CACHED: usize = 50_000;

impl InterMeasurer {
    pub fn new() -> InterMeasurer {
        let shapers = FACES
            .iter()
            .map(|face| {
                let font = FontRef::new(face.data).expect("embedded font is valid");
                let data = ShaperData::new(&font);
                (font, data)
            })
            .collect();
        InterMeasurer {
            base: DeterministicTextMeasurer::default(),
            state: Mutex::new(State {
                shapers,
                ratios: HashMap::new(),
            }),
        }
    }

    /// Inter's width for `text` divided by the deterministic measurer's width.
    fn ratio(&self, text: &str, style: &TextStyle) -> f64 {
        let face = fonts::face(style.font_weight.as_deref(), style.font_style.as_deref());
        let index = FACES
            .iter()
            .position(|f| f.bold == face.bold && f.italic == face.italic)
            .unwrap_or(0);
        let key = Key {
            text: text.to_string(),
            size: style.font_size.to_bits(),
            face: index,
        };
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(&ratio) = state.ratios.get(&key) {
            return ratio;
        }
        let size = if style.font_size > 0.0 { style.font_size } else { 16.0 };
        let (font, data) = &state.shapers[index];
        let inter = DeterministicTextMeasurer::normalized_text_lines(text)
            .iter()
            .map(|line| shaped_width(font, data, line, size))
            .fold(0.0, f64::max);
        let base = self.base.measure(text, style).width;
        let ratio = if base > 0.01 && inter > 0.0 { inter / base } else { 1.0 };
        if state.ratios.len() >= MAX_CACHED {
            state.ratios.clear();
        }
        state.ratios.insert(key, ratio);
        ratio
    }
}

/// Advance width of one line shaped with harfrust, which is what resvg draws text with.
fn shaped_width(font: &FontRef<'_>, data: &ShaperData, line: &str, size: f64) -> f64 {
    let text = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return 0.0;
    }
    let shaper = data.shaper(font).build();
    let mut buffer = UnicodeBuffer::new();
    buffer.push_str(&text);
    buffer.guess_segment_properties();
    let glyphs = shaper.shape(buffer, ShapeOptions::new());
    let advance: i64 = glyphs
        .glyph_positions()
        .iter()
        .map(|position| i64::from(position.x_advance))
        .sum();
    advance as f64 * size / f64::from(shaper.units_per_em().max(1))
}

fn scaled((left, right): (f64, f64), ratio: f64) -> (f64, f64) {
    (left * ratio, right * ratio)
}

impl TextMeasurer for InterMeasurer {
    fn cancellation_requested(&self) -> bool {
        self.base.cancellation_requested()
    }

    fn measure(&self, text: &str, style: &TextStyle) -> TextMetrics {
        let mut metrics = self.base.measure(text, style);
        metrics.width *= self.ratio(text, style);
        metrics
    }

    fn measure_svg_text_computed_length_px(&self, text: &str, style: &TextStyle) -> f64 {
        self.base.measure_svg_text_computed_length_px(text, style) * self.ratio(text, style)
    }

    fn measure_svg_text_bbox_x(&self, text: &str, style: &TextStyle) -> (f64, f64) {
        scaled(self.base.measure_svg_text_bbox_x(text, style), self.ratio(text, style))
    }

    fn measure_svg_text_bbox_x_with_ascii_overhang(
        &self,
        text: &str,
        style: &TextStyle,
    ) -> (f64, f64) {
        scaled(
            self.base.measure_svg_text_bbox_x_with_ascii_overhang(text, style),
            self.ratio(text, style),
        )
    }

    fn measure_svg_title_bbox_x(&self, text: &str, style: &TextStyle) -> (f64, f64) {
        scaled(self.base.measure_svg_title_bbox_x(text, style), self.ratio(text, style))
    }

    fn measure_svg_simple_text_bbox_width_px(&self, text: &str, style: &TextStyle) -> f64 {
        self.base.measure_svg_simple_text_bbox_width_px(text, style) * self.ratio(text, style)
    }

    fn measure_svg_raw_text_bbox_width_px(&self, text: &str, style: &TextStyle) -> f64 {
        self.base.measure_svg_raw_text_bbox_width_px(text, style) * self.ratio(text, style)
    }

    fn measure_svg_raw_text_bbox_height_px(&self, text: &str, style: &TextStyle) -> f64 {
        self.base.measure_svg_raw_text_bbox_height_px(text, style)
    }

    fn measure_svg_text_bounding_client_rect_width_px(&self, text: &str, style: &TextStyle) -> f64 {
        self.base.measure_svg_text_bounding_client_rect_width_px(text, style)
            * self.ratio(text, style)
    }

    fn measure_svg_tspan_text_bbox_width_px(&self, text: &str, style: &TextStyle) -> f64 {
        self.base.measure_svg_tspan_text_bbox_width_px(text, style) * self.ratio(text, style)
    }

    fn measure_svg_tspan_text_bbox_height_px(&self, text: &str, style: &TextStyle) -> f64 {
        self.base.measure_svg_tspan_text_bbox_height_px(text, style)
    }

    fn measure_svg_create_text_bbox_y_offset_px(&self, text: &str, style: &TextStyle) -> f64 {
        self.base.measure_svg_create_text_bbox_y_offset_px(text, style)
    }

    fn measure_svg_create_text_middle_bbox_y_offset_px(
        &self,
        text: &str,
        style: &TextStyle,
    ) -> f64 {
        self.base.measure_svg_create_text_middle_bbox_y_offset_px(text, style)
    }

    fn measure_svg_simple_text_bbox_width_for_wrap_px(
        &self,
        text: &str,
        style: &TextStyle,
    ) -> f64 {
        self.base.measure_svg_simple_text_bbox_width_for_wrap_px(text, style)
            * self.ratio(text, style)
    }

    fn measure_mermaid_calculate_text_dimensions(
        &self,
        text: &str,
        style: &TextStyle,
    ) -> TextMetrics {
        let mut metrics = self.base.measure_mermaid_calculate_text_dimensions(text, style);
        metrics.width *= self.ratio(text, style);
        metrics
    }

    fn measure_canvas_text_width_px(&self, text: &str, style: &TextStyle) -> f64 {
        self.base.measure_canvas_text_width_px(text, style) * self.ratio(text, style)
    }

    fn measure_svg_simple_text_bbox_height_px(&self, text: &str, style: &TextStyle) -> f64 {
        self.base.measure_svg_simple_text_bbox_height_px(text, style)
    }

    fn measure_wrapped(
        &self,
        text: &str,
        style: &TextStyle,
        max_width: Option<f64>,
        wrap_mode: WrapMode,
    ) -> TextMetrics {
        // Wrap where Inter's text would reach the limit, then report Inter widths.
        let ratio = self.ratio(text, style);
        let mut metrics =
            self.base
                .measure_wrapped(text, style, max_width.map(|w| w / ratio), wrap_mode);
        metrics.width *= ratio;
        metrics
    }

    fn measure_wrapped_with_raw_width(
        &self,
        text: &str,
        style: &TextStyle,
        max_width: Option<f64>,
        wrap_mode: WrapMode,
    ) -> (TextMetrics, Option<f64>) {
        let ratio = self.ratio(text, style);
        let (mut metrics, raw) = self.base.measure_wrapped_with_raw_width(
            text,
            style,
            max_width.map(|w| w / ratio),
            wrap_mode,
        );
        metrics.width *= ratio;
        (metrics, raw.map(|w| w * ratio))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inter_width(measurer: &InterMeasurer, face: usize, text: &str) -> f64 {
        let state = measurer.state.lock().unwrap();
        let (font, data) = &state.shapers[face];
        shaped_width(font, data, text, 16.0)
    }

    #[test]
    fn widths_come_from_inter_and_heights_from_merman() {
        let measurer = InterMeasurer::new();
        let style = TextStyle::default();
        let text = "Rasterize with resvg";
        let metrics = measurer.measure(text, &style);
        assert!((metrics.width - inter_width(&measurer, 0, text)).abs() < 0.01);
        assert_eq!(metrics.height, measurer.base.measure(text, &style).height);
    }

    #[test]
    fn bold_labels_use_the_bold_face() {
        let measurer = InterMeasurer::new();
        let bold = TextStyle {
            font_weight: Some("bold".to_string()),
            ..TextStyle::default()
        };
        let text = "Engine";
        let width = measurer.measure(text, &bold).width;
        assert!((width - inter_width(&measurer, 2, text)).abs() < 0.01);
        assert!(width > measurer.measure(text, &TextStyle::default()).width);
    }

    #[test]
    fn multi_line_labels_use_the_widest_line() {
        let measurer = InterMeasurer::new();
        let width = measurer
            .measure("short<br>a much longer line", &TextStyle::default())
            .width;
        assert!((width - inter_width(&measurer, 0, "a much longer line")).abs() < 0.01);
    }

    #[test]
    fn repeated_measurements_are_cached() {
        let measurer = InterMeasurer::new();
        let style = TextStyle::default();
        let first = measurer.measure("cached", &style);
        let second = measurer.measure("cached", &style);
        assert_eq!(first.width, second.width);
        assert_eq!(measurer.state.lock().unwrap().ratios.len(), 1);
    }
}
