# mer

Render [Mermaid](https://mermaid.js.org) diagrams as real graphics in the terminal.

```sh
mer flow.mmd              # inline, sized to your terminal's text
mer README.md             # every mermaid block in a Markdown file
cat flow.mmd | mer -      # stdin
mer -w flow.mmd           # redraw whenever the file changes
mer -i big.mmd            # full-screen viewer to pan and zoom
```

There is no browser or Node.js involved. Diagrams are laid out by
[merman](https://github.com/Latias94/merman), a Rust implementation of Mermaid, drawn with
[resvg](https://github.com/linebender/resvg) using the embedded Inter font, and shown through the
kitty graphics protocol. A typical diagram is on screen about 20 ms after you press Enter.

## Requirements

- A terminal with the kitty graphics protocol and its Unicode placeholders for images.
  [Ghostty](https://ghostty.org) is the primary target; kitty also works. Other terminals get
  diagrams drawn with box-drawing characters instead.
- Rust 1.95 or newer to build. `rust-toolchain.toml` pins the version rustup installs.

Check what `mer` detects in your terminal:

```sh
mer --doctor
```

## Install

```sh
cargo install --path .
```

## Usage

### Files and Markdown

`.mmd` and `.mermaid` files hold one diagram. In `.md` files, every `mermaid` code block is
drawn in order, with the nearest heading above it as a caption. Pick one with `-n`:

```sh
mer docs/architecture.md -n 2
```

Diagrams come out the size of your terminal's text and shrink to fit its width. Tall diagrams
scroll, like any other output. `--scale` makes them larger or smaller, and `--fit contain` keeps
them within the screen.

### stdin

`mer -` reads stdin, and so does `mer` with no arguments when stdin is a pipe. The format is
detected: input starting with a Mermaid header is a diagram; anything else is treated as
Markdown. `--stdin-format` overrides detection.

Input that ends quickly is drawn once, exactly like a file. Input that keeps arriving, such as a
generator or an LLM writing a diagram, is shown live: the diagram redraws in place as lines
arrive, and the final picture is printed when the input ends. Markdown streams print each
diagram as soon as its code block closes. NUL bytes separate documents, which are drawn one
after another.

### Watch

`mer -w FILE…` keeps the diagram on screen and redraws it when a file changes, which works well
in a split next to your editor. Syntax errors appear under the last good picture.

| Key | Action |
|---|---|
| `n` / `p` | Next / previous diagram in a Markdown file |
| `r` | Reload |
| `q`, `Esc` | Quit, leaving the picture in the scrollback |

### Viewer

`mer -i` opens a full-screen viewer. Only the visible part is rendered at the current zoom, so
large diagrams stay sharp. Add `-w` to reload when files change.

| Key | Action |
|---|---|
| Arrows, `h` `j` `k` `l`, drag | Pan (`H` `J` `K` `L` pan further) |
| `+` / `-`, scroll wheel | Zoom (the wheel zooms around the pointer) |
| `0` / `1` / `w` | Fit the diagram / actual size / fit the width |
| `n` / `p`, `Home` / `End` | Next / previous / first / last diagram |
| `r` | Reload |
| `q`, `Esc` | Quit |

### Themes

The default `terminal` theme derives every color from your terminal's foreground, background
and ANSI palette. It follows light/dark switches while `mer` is running. Mermaid's own themes are
available with `-t`: `default`, `dark`, `forest`, `neutral`, `base`, `neo` and `neo-dark`.

`-b` sets the background: `transparent`, `terminal`, or a color such as `#1e1e2e`. `-c` merges a
Mermaid configuration file in JSON. Settings inside a diagram, in frontmatter or `%%{init}%%`
directives, take precedence over both.

### Without graphics

In terminals without the kitty graphics protocol, `mer` draws diagrams with box-drawing
characters and says so on stderr. `--protocol text` does the same on purpose, including when
output goes to a pipe or a CI log. A drawing wider than the terminal is reported as an error,
because wrapped lines would garble it; output to a pipe or file has no width limit. Text
renderings exist for flowcharts, sequence, class and ER diagrams, gantt charts, journeys, git
graphs, mindmaps, timelines, XY charts, packet and kanban diagrams, and for state diagrams with
simple layouts. For other types, write an image with `-o`.

### tmux

Images work inside tmux 3.3 or newer once passthrough is allowed:

```tmux
set -g allow-passthrough on
```

### Export and checks

```sh
mer flow.mmd -o flow.png          # 2x PNG; --scale changes the size
mer docs/guide.md -o guide.svg    # guide-1.svg, guide-2.svg, …
mer --check docs/*.md             # report syntax errors; exits 1 if any
```

Exports use Mermaid's `default` theme on white unless `-t` or `-b` say otherwise.

## Configuration

Defaults can be set in `~/.config/mer/config.toml`, or `$XDG_CONFIG_HOME/mer/config.toml`, or a
file named by `MER_CONFIG`. Flags override it.

```toml
theme = "terminal"
background = "transparent"
scale = 1.2
fit = "width"          # width, contain or none
protocol = "auto"      # auto, kitty or text

[mermaid]              # Mermaid configuration; -c merges over it
flowchart.curve = "basis"
```

`mer --doctor` shows which configuration file is in use.

## Limitations

- C4 diagrams use Mermaid's fixed C4 colors, which are hard to read on dark backgrounds.
- Inside tmux, pointer positions arrive in whole cells rather than pixels, so dragging and
  zooming with the mouse in the viewer are slightly coarser.
- merman is pre-1.0. Its output is very close to mermaid.js, but not identical.
- Very large diagrams take longer to draw: an ER schema of 250 tables takes about a second.

## Development

```sh
cargo test                                   # unit and end-to-end tests
cargo test --test e2e gallery -- --ignored   # contact sheets of every diagram type
MER_E2E_DUMP=1 cargo test --test e2e         # save decoded frames to target/tmp/e2e
```

The end-to-end tests run `mer` on a pseudo-terminal that answers its queries like Ghostty does,
including behind a real tmux when it is installed, then decode the images and placeholder grids
it writes.

The Inter font is licensed under the SIL Open Font License 1.1; see
`assets/fonts/LICENSE-Inter.txt`.
