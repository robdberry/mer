mod cli;
mod diag;
mod display;
mod engine;
mod fonts;
mod input;
mod live;
mod raster;
mod render;
mod size;
mod term;
mod theme;

use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser};

use cli::{Cli, OutputFormat, StdinFormat};
use display::Setup;
use engine::{Control, Engine};
use input::{Diagram, Format};
use raster::Rasterizer;
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
        return display::doctor();
    }
    if cli.inputs.is_empty() && io::stdin().is_terminal() {
        Cli::command().print_help()?;
        return Ok(ExitCode::from(2));
    }
    let only_stdin = match cli.inputs.as_slice() {
        [] => true,
        [path] => path.as_os_str() == "-",
        _ => false,
    };
    if cli.watch {
        if cli.inputs.is_empty() || cli.inputs.iter().any(|path| path.as_os_str() == "-") {
            bail!("--watch needs files; input from stdin is already shown as it arrives");
        }
        if cli.check || cli.output.is_some() {
            bail!("--watch cannot be combined with --check or -o");
        }
        return live::watch::run(Setup::new(cli)?, cli.inputs.clone(), cli.diagram);
    }
    if cli.check {
        return check(cli, &load(cli)?);
    }
    if let Some(output) = &cli.output {
        return export(cli, &load(cli)?, output);
    }
    if only_stdin && cli.diagram.is_none() {
        return live::stream::run(Setup::new(cli)?, stdin_format(cli));
    }
    let diagrams = load(cli)?;
    display::show(&Setup::new(cli)?, &diagrams)
}

fn stdin_format(cli: &Cli) -> Option<Format> {
    match cli.stdin_format {
        StdinFormat::Auto => None,
        StdinFormat::Mermaid => Some(Format::Mermaid),
        StdinFormat::Markdown => Some(Format::Markdown),
    }
}

/// Reads every input to the end.
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
            let mut bytes = Vec::new();
            io::stdin()
                .lock()
                .read_to_end(&mut bytes)
                .context("cannot read stdin")?;
            let text = String::from_utf8_lossy(&bytes);
            ("<stdin>".to_string(), input::documents(&text, stdin_format(cli)))
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

fn no_diagrams() -> ExitCode {
    eprintln!("mer: no Mermaid diagram found");
    ExitCode::from(1)
}

fn check(cli: &Cli, diagrams: &[Diagram]) -> Result<ExitCode> {
    if diagrams.is_empty() {
        return Ok(no_diagrams());
    }
    let user = display::user_config(cli.config.as_deref())?;
    let theme = cli.theme.as_deref().unwrap_or("default");
    let engine = Engine::new(display::site_config(theme, None, user.as_ref()), None);
    let color = io::stderr().is_terminal();
    let failures = diagrams
        .iter()
        .filter(|diagram| match engine.render_svg(&diagram.text, Control::new()) {
            Ok(_) => false,
            Err(failure) => {
                display::report(&failure, diagram, color);
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
    if diagrams.is_empty() {
        return Ok(no_diagrams());
    }
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
    let theme = cli.theme.as_deref().unwrap_or("default");
    let background = display::background(cli.background.as_deref(), theme, None)?;
    let svg_background = match format {
        OutputFormat::Svg => background.map(Rgb::hex),
        OutputFormat::Png => None,
    };
    let user = display::user_config(cli.config.as_deref())?;
    let config = display::site_config(theme, None, user.as_ref());
    let engine = Engine::new(config, svg_background.as_deref());
    let mut rasterizer = Rasterizer::new();
    let color = io::stderr().is_terminal();
    let mut failed = false;

    for (index, diagram) in diagrams.iter().enumerate() {
        let svg = match engine.render_svg(&diagram.text, Control::new()) {
            Ok(svg) => svg,
            Err(failure) => {
                display::report(&failure, diagram, color);
                failed = true;
                continue;
            }
        };
        let bytes = match format {
            OutputFormat::Svg => svg.into_bytes(),
            OutputFormat::Png => {
                let tree = rasterizer.parse(&svg)?;
                let (w, h) = (tree.size().width(), tree.size().height());
                let scale = (EXPORT_SCALE * cli.scale).min((EXPORT_MAX_PIXELS / (w * h)).sqrt());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_outputs() {
        assert_eq!(numbered(Path::new("out.png"), 0, 1), PathBuf::from("out.png"));
        assert_eq!(numbered(Path::new("dir/out.png"), 1, 3), PathBuf::from("dir/out-2.png"));
        assert_eq!(numbered(Path::new("out"), 0, 2), PathBuf::from("out-1"));
    }
}
