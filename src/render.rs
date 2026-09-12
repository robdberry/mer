//! One diagram to one terminal frame: Mermaid source, then SVG, then pixels sized to the grid.

use std::io;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::diag::Diagnostic;
use crate::engine::{Control, Engine, Failure};
use crate::raster::{self, Rasterizer};
use crate::size::{self, Fit, Grid};
use crate::term::kitty;
use crate::theme::Rgb;

pub struct Frame {
    /// Straight RGBA pixels.
    pub rgba: Vec<u8>,
    /// Size in device pixels: whole cells.
    pub pixels: (u32, u32),
    pub cols: u16,
    pub rows: u16,
    pub elapsed: Duration,
}

pub struct Renderer {
    engine: Engine,
    rasterizer: Rasterizer,
    background: Option<Rgb>,
}

impl Renderer {
    pub fn new(config: Value, background: Option<Rgb>) -> Renderer {
        Renderer {
            engine: Engine::new(config, None),
            rasterizer: Rasterizer::new(),
            background,
        }
    }

    pub fn frame(
        &mut self,
        source: &str,
        grid: &Grid,
        scale: f32,
        fit: Fit,
        control: Control,
    ) -> Result<Frame, Failure> {
        let started = Instant::now();
        let svg = self.engine.render_svg(source, control)?;
        let tree = self.rasterizer.parse(&svg).map_err(|err| {
            Failure::Diagnostic(Diagnostic {
                message: format!("{err:#}"),
                span: None,
            })
        })?;
        let plan = size::plan((tree.size().width(), tree.size().height()), grid, scale, fit);
        let pixels = plan.pixels(grid);
        let rgba = raster::straight_rgba(&raster::render(&tree, plan.scale, pixels, self.background));
        Ok(Frame {
            rgba,
            pixels,
            cols: plan.cols,
            rows: plan.rows,
            elapsed: started.elapsed(),
        })
    }
}

/// Appends an image as image `id` followed by its placeholder grid, one line per row.
pub fn write_inline(out: &mut Vec<u8>, frame: &Frame, id: u32) -> io::Result<()> {
    kitty::transmit_virtual(out, id, &frame.rgba, frame.pixels, (frame.cols, frame.rows))?;
    write_cells(out, id, frame.cols, frame.rows);
    Ok(())
}

/// Appends the placeholder grid for an image that has already been transmitted.
pub fn write_cells(out: &mut Vec<u8>, id: u32, cols: u16, rows: u16) {
    let mut cells = String::new();
    for row in 0..rows {
        kitty::placeholder_row(&mut cells, id, row, cols);
        cells.push('\n');
    }
    out.extend_from_slice(cells.as_bytes());
}
