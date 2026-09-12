//! How big a diagram is drawn: text-matched scale, fitting, and snapping to whole cells.

use clap::ValueEnum;
use serde::Deserialize;

use crate::term::kitty::MAX_CELLS;

/// Terminal grid geometry, with cell sizes in device pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    pub cell_w: u32,
    pub cell_h: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fit {
    /// Shrink to the terminal width; the height may scroll.
    Width,
    /// Shrink to fit both the width and the height.
    Contain,
    /// Keep the text-matched size.
    None,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    /// Device pixels per SVG user unit.
    pub scale: f32,
    pub cols: u16,
    pub rows: u16,
}

impl Plan {
    /// Frame size in device pixels. Frames cover whole cells so the terminal never resamples.
    pub fn pixels(&self, grid: &Grid) -> (u32, u32) {
        (
            u32::from(self.cols) * grid.cell_w,
            u32::from(self.rows) * grid.cell_h,
        )
    }
}

/// Mermaid's default label size, in SVG user units.
const LABEL_PX: f32 = 16.0;
/// Typical monospace line height as a multiple of the font size.
const LINE_HEIGHT: f32 = 1.3;
/// Largest frame, in pixels.
const MAX_PIXELS: f32 = 16_000_000.0;

/// The scale at which Mermaid's labels come out the size of the terminal's own text.
pub fn text_matched_scale(cell_h: u32) -> f32 {
    cell_h as f32 / (LINE_HEIGHT * LABEL_PX)
}

/// Chooses the scale and cell footprint for an SVG of `size` user units. `scale` multiplies
/// the text-matched scale.
pub fn plan(size: (f32, f32), grid: &Grid, scale: f32, fit: Fit) -> Plan {
    let (w, h) = (size.0.max(1.0), size.1.max(1.0));
    let (cell_w, cell_h) = (grid.cell_w.max(1) as f32, grid.cell_h.max(1) as f32);
    let max_cols = match fit {
        Fit::None => MAX_CELLS,
        Fit::Width | Fit::Contain => grid.cols.clamp(1, MAX_CELLS),
    };
    let max_rows = match fit {
        Fit::Contain => grid.rows.clamp(1, MAX_CELLS),
        Fit::Width | Fit::None => MAX_CELLS,
    };
    let mut s = (text_matched_scale(grid.cell_h.max(1)) * scale)
        .min(f32::from(max_cols) * cell_w / w)
        .min(f32::from(max_rows) * cell_h / h);
    let pixels = w * s * h * s;
    if pixels > MAX_PIXELS {
        s *= (MAX_PIXELS / pixels).sqrt();
    }
    Plan {
        scale: s,
        cols: cells(w * s, cell_w).min(max_cols),
        rows: cells(h * s, cell_h).min(max_rows),
    }
}

fn cells(px: f32, cell: f32) -> u16 {
    // The epsilon keeps exact fits from spilling into an extra cell through float error.
    (px / cell - 1e-3).ceil().clamp(1.0, f32::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    const RETINA: Grid = Grid {
        cols: 120,
        rows: 40,
        cell_w: 17,
        cell_h: 34,
    };

    #[test]
    fn labels_match_terminal_text() {
        let s = text_matched_scale(34);
        assert!((s - 1.6346).abs() < 1e-3);
        assert!((16.0 * s * 1.3 - 34.0).abs() < 1e-3);
    }

    #[test]
    fn small_diagrams_keep_text_matched_size() {
        let plan = plan((200.0, 100.0), &RETINA, 1.0, Fit::Width);
        assert!((plan.scale - text_matched_scale(34)).abs() < 1e-6);
        assert_eq!((plan.cols, plan.rows), (20, 5));
        assert_eq!(plan.pixels(&RETINA), (340, 170));
    }

    #[test]
    fn wide_diagrams_shrink_to_the_terminal_width() {
        let plan = plan((4000.0, 500.0), &RETINA, 1.0, Fit::Width);
        assert_eq!(plan.cols, 120);
        assert!(4000.0 * plan.scale <= 120.0 * 17.0 + 1e-3);
    }

    #[test]
    fn tall_diagrams_scroll_unless_contained() {
        let tall = (300.0, 3000.0);
        assert!(plan(tall, &RETINA, 1.0, Fit::Width).rows > RETINA.rows);
        let contained = plan(tall, &RETINA, 1.0, Fit::Contain);
        assert_eq!(contained.rows, RETINA.rows);
        assert!(contained.cols <= RETINA.cols);
    }

    #[test]
    fn user_scale_multiplies_but_fit_still_applies() {
        let base = plan((200.0, 100.0), &RETINA, 1.0, Fit::Width);
        let doubled = plan((200.0, 100.0), &RETINA, 2.0, Fit::Width);
        assert!((doubled.scale - 2.0 * base.scale).abs() < 1e-6);
        assert_eq!(plan((200.0, 100.0), &RETINA, 50.0, Fit::Width).cols, 120);
    }

    #[test]
    fn placeholder_and_pixel_limits_hold() {
        let huge = Grid {
            cols: 400,
            rows: 400,
            cell_w: 17,
            cell_h: 34,
        };
        let p = plan((20000.0, 20000.0), &huge, 1.0, Fit::None);
        assert!(p.cols <= MAX_CELLS && p.rows <= MAX_CELLS);
        let (w, h) = p.pixels(&huge);
        assert!(f64::from(w) * f64::from(h) <= 16_000_000.0 * 1.1);
    }

    #[test]
    fn degenerate_inputs_still_produce_a_cell() {
        let tiny = Grid {
            cols: 0,
            rows: 0,
            cell_w: 0,
            cell_h: 0,
        };
        let p = plan((0.0, 0.0), &tiny, 1.0, Fit::Contain);
        assert!(p.cols >= 1 && p.rows >= 1 && p.scale.is_finite());
    }
}
