mod cli;
mod diag;
mod engine;
mod fonts;
mod input;
mod raster;
mod size;
mod term;
mod theme;

use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser};
use serde_json::{Value, json};

use cli::{Cli, FitArg, OutputFormat, Protocol, StdinFormat};
use diag::Diagnostic;
use engine::{Control, Engine, Failure};
use input::{Diagram, Format};
use raster::Rasterizer;
use size::{Fit, Grid};
use term::kitty;
use term::probe::{self, Caps};
use term::tty::Tty;
use theme::Rgb;

/// Scale for exported PNGs before `--scale`, so they are sharp on high-density displays.
const EXPORT_SCALE: f32 = 2.0;
/// Largest exported PNG, in pixels.
const EXPORT_MAX_PIXELS: f32 = 64_000_000.0;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("mer: {err:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: &Cli) -> Result<ExitCode> {
    if cli.doctor {
        return doctor();
    }
    if cli.inputs.is_empty() && io::stdin().is_terminal() {
        Cli::command().print_help()?;
        return Ok(ExitCode::from(2));
    }
    let diagrams = load(cli)?;
    if diagrams.is_empty() {
        eprintln!("mer: no Mermaid diagram found");
        return Ok(ExitCode::from(1));
    }
    if cli.check {
        check(cli, &diagrams)
    } else if let Some(output) = &cli.output {
        export(cli, &diagrams, output)
    } else {
        display(cli, &diagrams)
    }
}

fn load(cli: &Cli) -> Result<Vec<Diagram>> {
    let inputs = if cli.inputs.is_empty() {
        vec![PathBuf::from("-")]
    } else {
        cli.inputs.clone()
    };
    let mut diagrams = Vec::new();
    let mut read_stdin = false;
    for path in &inputs {
        let (origin, found) = if path.as_os_str() == "-" {
            if std::mem::replace(&mut read_stdin, true) {
                bail!("stdin can only be read once");
            }
            ("<stdin>".to_string(), stdin_diagrams(cli)?)
        } else {
            let text = fs::read_to_string(path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            let origin = path.display().to_string();
            let found = input::diagrams(&text, &origin, input::format_from_extension(path));
            (origin, found)
        };
        match cli.diagram {
            None => diagrams.extend(found),
            Some(n) => {
                let count = found.len();
                let selected = found.into_iter().nth(n.saturating_sub(1));
                diagrams.push(selected.with_context(|| {
                    format!("{origin} has {count} diagram(s), so there is no diagram {n}")
                })?);
            }
        }
    }
    Ok(diagrams)
}

/// Reads stdin to the end. NUL bytes separate documents; only the last one is kept unless
/// `--append` is given, so the result does not depend on how fast the input arrived.
fn stdin_diagrams(cli: &Cli) -> Result<Vec<Diagram>> {
    let mut bytes = Vec::new();
    io::stdin()
        .lock()
        .read_to_end(&mut bytes)
        .context("cannot read stdin")?;
    let text = String::from_utf8_lossy(&bytes);
    let format = match cli.stdin_format {
        StdinFormat::Auto => None,
        StdinFormat::Mermaid => Some(Format::Mermaid),
        StdinFormat::Markdown => Some(Format::Markdown),
    };
    let mut documents: Vec<&str> = text.split('\0').filter(|d| !d.trim().is_empty()).collect();
    if !cli.append {
        documents = documents.split_off(documents.len().saturating_sub(1));
    }
    Ok(documents
        .into_iter()
        .flat_map(|document| input::diagrams(document, "<stdin>", format))
        .collect())
}

fn check(cli: &Cli, diagrams: &[Diagram]) -> Result<ExitCode> {
    let engine = Engine::new(site_config(cli, None, true)?, None);
    let color = io::stderr().is_terminal();
    let failures = diagrams
        .iter()
        .filter(|diagram| match engine.render_svg(&diagram.text, Control::new()) {
            Ok(_) => false,
            Err(failure) => {
                report(&failure, diagram, color);
                true
            }
        })
        .count();
    if failures == 0 {
        return Ok(ExitCode::SUCCESS);
    }
    eprintln!("{failures} of {} diagram(s) failed", diagrams.len());
    Ok(ExitCode::from(1))
}

fn export(cli: &Cli, diagrams: &[Diagram], output: &Path) -> Result<ExitCode> {
    let to_stdout = output.as_os_str() == "-";
    let format = match (cli.format, output.extension().and_then(|e| e.to_str())) {
        (Some(format), _) => format,
        (None, Some(ext)) if ext.eq_ignore_ascii_case("png") => OutputFormat::Png,
        (None, Some(ext)) if ext.eq_ignore_ascii_case("svg") => OutputFormat::Svg,
        _ => bail!("cannot tell the image format of {}; use --format", output.display()),
    };
    if to_stdout && diagrams.len() > 1 {
        bail!("{} diagrams found; pick one with -n to write to stdout", diagrams.len());
    }
    let background = background(cli, None, "default")?;
    let svg_background = match format {
        OutputFormat::Svg => background.map(Rgb::hex),
        OutputFormat::Png => None,
    };
    let engine = Engine::new(site_config(cli, None, true)?, svg_background.as_deref());
    let mut rasterizer = Rasterizer::new();
    let color = io::stderr().is_terminal();
    let mut failed = false;

    for (index, diagram) in diagrams.iter().enumerate() {
        let svg = match engine.render_svg(&diagram.text, Control::new()) {
            Ok(svg) => svg,
            Err(failure) => {
                report(&failure, diagram, color);
                failed = true;
                continue;
            }
        };
        let bytes = match format {
            OutputFormat::Svg => svg.into_bytes(),
            OutputFormat::Png => {
                let tree = rasterizer.parse(&svg)?;
                let (w, h) = (tree.size().width(), tree.size().height());
                let mut scale = EXPORT_SCALE * cli.scale;
                scale = scale.min((EXPORT_MAX_PIXELS / (w * h)).sqrt());
                let size = ((w * scale).ceil() as u32, (h * scale).ceil() as u32);
                raster::render(&tree, scale, size, background)
                    .encode_png()
                    .context("cannot encode PNG")?
            }
        };
        if to_stdout {
            io::stdout().lock().write_all(&bytes)?;
        } else {
            let path = numbered(output, index, diagrams.len());
            fs::write(&path, bytes).with_context(|| format!("cannot write {}", path.display()))?;
        }
    }
    Ok(if failed { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

/// `out.png` for a single diagram; `out-1.png`, `out-2.png`, … for several.
fn numbered(output: &Path, index: usize, count: usize) -> PathBuf {
    if count == 1 {
        return output.to_path_buf();
    }
    let stem = output.file_stem().unwrap_or_default().to_string_lossy();
    let name = match output.extension() {
        Some(ext) => format!("{stem}-{}.{}", index + 1, ext.to_string_lossy()),
        None => format!("{stem}-{}", index + 1),
    };
    output.with_file_name(name)
}

fn display(cli: &Cli, diagrams: &[Diagram]) -> Result<ExitCode> {
    let forced = cli.protocol == Protocol::Kitty;
    if !forced && !io::stdout().is_terminal() {
        bail!("stdout is not a terminal; write an image with -o FILE, or force output with --protocol kitty");
    }
    let caps = match Tty::open() {
        Ok(tty) => probe::probe(&tty, probe_timeout())?,
        Err(_) if forced => Caps::default(),
        Err(err) => return Err(err).context("cannot open the terminal (/dev/tty)"),
    };
    if !forced && !caps.kitty_graphics {
        bail!(
            "this terminal does not support the kitty graphics protocol; \
             use a terminal that does (such as Ghostty or kitty), or write an image with -o FILE"
        );
    }

    let (cell_w, cell_h) = caps.cell.unwrap_or((10, 20));
    let grid = Grid {
        cols: if caps.cols == 0 { 80 } else { caps.cols },
        rows: if caps.rows == 0 { 24 } else { caps.rows },
        cell_w,
        cell_h,
    };
    let fit = match cli.fit {
        FitArg::Width => Fit::Width,
        FitArg::Contain => Fit::Contain,
        FitArg::None => Fit::None,
    };
    let theme = cli.theme.as_deref().unwrap_or("terminal");
    let background = background(cli, Some(&caps), theme)?;
    let engine = Engine::new(site_config(cli, Some(&caps), false)?, None);
    let mut rasterizer = Rasterizer::new();
    let captions = diagrams.len() > 1;
    let color = io::stderr().is_terminal();
    let mut stdout = io::stdout().lock();
    let mut failed = false;

    for diagram in diagrams {
        let svg = match engine.render_svg(&diagram.text, Control::new()) {
            Ok(svg) => svg,
            Err(failure) => {
                stdout.flush()?;
                report(&failure, diagram, color);
                failed = true;
                continue;
            }
        };
        let tree = rasterizer.parse(&svg)?;
        let plan = size::plan(
            (tree.size().width(), tree.size().height()),
            &grid,
            cli.scale,
            fit,
        );
        let pixels = plan.pixels(&grid);
        let rgba = raster::straight_rgba(&raster::render(&tree, plan.scale, pixels, background));

        let mut out = Vec::with_capacity(rgba.len() / 4);
        out.extend_from_slice(b"\x1b[?2026h");
        if let (true, Some(caption)) = (captions, &diagram.caption) {
            writeln!(out, "\x1b[2m{caption}\x1b[22m")?;
        }
        let id = kitty::random_id();
        kitty::transmit_virtual(&mut out, id, &rgba, pixels, (plan.cols, plan.rows))?;
        let mut cells = String::new();
        for row in 0..plan.rows {
            kitty::placeholder_row(&mut cells, id, row, plan.cols);
            cells.push('\n');
        }
        out.extend_from_slice(cells.as_bytes());
        out.extend_from_slice(b"\x1b[?2026l");
        stdout.write_all(&out)?;
        stdout.flush()?;
    }
    Ok(if failed { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

fn doctor() -> Result<ExitCode> {
    let tty = Tty::open().context("cannot open the terminal (/dev/tty)")?;
    let caps = probe::probe(&tty, probe_timeout())?;
    let color = |rgb: Option<Rgb>| rgb.map_or_else(|| "not reported".to_string(), Rgb::hex);
    let program = std::env::var("TERM_PROGRAM").unwrap_or_else(|_| "unknown".to_string());
    let version = std::env::var("TERM_PROGRAM_VERSION").unwrap_or_default();
    println!("terminal          {program} {version}");
    println!("responded         {}", if caps.responded { "yes" } else { "no" });
    println!("kitty graphics    {}", if caps.kitty_graphics { "yes" } else { "no" });
    println!("grid              {} × {} cells", caps.cols, caps.rows);
    match caps.cell {
        Some((w, h)) => println!(
            "cell size         {w} × {h} px (text-matched scale {:.2})",
            size::text_matched_scale(h)
        ),
        None => println!("cell size         not reported"),
    }
    println!("foreground        {}", color(caps.palette.fg));
    println!("background        {}", color(caps.palette.bg));
    let scheme = match caps.dark {
        Some(true) => "dark",
        Some(false) => "light",
        None => "not reported",
    };
    println!("color scheme      {scheme}");
    Ok(ExitCode::SUCCESS)
}

fn probe_timeout() -> Duration {
    let millis = std::env::var("MER_PROBE_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(500);
    Duration::from_millis(millis)
}

/// Mermaid site configuration: the theme, the embedded font, then the user's config file.
fn site_config(cli: &Cli, caps: Option<&Caps>, exporting: bool) -> Result<Value> {
    let default_theme = if exporting { "default" } else { "terminal" };
    let mut config = match cli.theme.as_deref().unwrap_or(default_theme) {
        "terminal" => caps
            .and_then(|caps| theme::terminal_theme(&caps.palette))
            .unwrap_or_else(|| {
                let dark = caps.and_then(|caps| caps.dark).unwrap_or(false);
                json!({ "theme": if dark { "dark" } else { "default" } })
            }),
        name => json!({ "theme": name }),
    };
    config["fontFamily"] = json!(fonts::FAMILY);
    if let Some(path) = &cli.config {
        let text = fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let user: Value = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", path.display()))?;
        merge(&mut config, user);
    }
    Ok(config)
}

fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// The frame background. The `terminal` theme draws on the terminal's own background; Mermaid
/// themes get the background they were designed for unless `-b` says otherwise.
fn background(cli: &Cli, caps: Option<&Caps>, theme: &str) -> Result<Option<Rgb>> {
    let spec = match cli.background.as_deref() {
        Some(spec) => spec,
        None if theme == "terminal" => "transparent",
        None if theme.contains("dark") => "#333333",
        None => "white",
    };
    Ok(match spec {
        "transparent" | "none" => None,
        "terminal" => Some(
            caps.and_then(|caps| caps.palette.bg)
                .context("the terminal did not report its background color")?,
        ),
        "white" => Some(Rgb(255, 255, 255)),
        "black" => Some(Rgb(0, 0, 0)),
        other => Some(Rgb::parse_x11(other).with_context(|| {
            format!("unknown background {other:?}; use transparent, terminal, or a #rrggbb color")
        })?),
    })
}

fn report(failure: &Failure, diagram: &Diagram, color: bool) {
    let diagnostic = match failure {
        Failure::Diagnostic(diagnostic) => diagnostic.clone(),
        Failure::Empty => Diagnostic {
            message: "no Mermaid diagram found".to_string(),
            span: None,
        },
        Failure::Cancelled => return,
    };
    eprint!("{}", diag::format(&diagnostic, diagram, color));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_overlays_nested_objects() {
        let mut base = json!({ "theme": "base", "themeVariables": { "a": 1, "b": 2 } });
        merge(&mut base, json!({ "themeVariables": { "b": 3 }, "flowchart": { "curve": "basis" } }));
        assert_eq!(
            base,
            json!({ "theme": "base", "themeVariables": { "a": 1, "b": 3 }, "flowchart": { "curve": "basis" } })
        );
    }

    #[test]
    fn numbered_outputs() {
        assert_eq!(numbered(Path::new("out.png"), 0, 1), PathBuf::from("out.png"));
        assert_eq!(numbered(Path::new("dir/out.png"), 1, 3), PathBuf::from("dir/out-2.png"));
        assert_eq!(numbered(Path::new("out"), 0, 2), PathBuf::from("out-1"));
    }
}
