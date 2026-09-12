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
kitty graphics protocol.

## Requirements

- A terminal that supports the kitty graphics protocol with Unicode placeholders.
  [Ghostty](https://ghostty.org) is the primary target; kitty also works.
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

### Export and checks

```sh
mer flow.mmd -o flow.png          # 2x PNG; --scale changes the size
mer docs/guide.md -o guide.svg    # guide-1.svg, guide-2.svg, …
mer --check docs/*.md             # report syntax errors; exits 1 if any
```

Exports use Mermaid's `default` theme on white unless `-t` or `-b` say otherwise.

## Limitations

- Terminals without the kitty graphics protocol aren't supported yet, and neither is tmux.
- C4 diagrams use Mermaid's fixed C4 colors, which are hard to read on dark backgrounds.
- merman is pre-1.0. Its output is very close to mermaid.js, but not identical.

## Development

```sh
cargo test                                   # unit and end-to-end tests
cargo test --test e2e gallery -- --ignored   # contact sheets of every diagram type
MER_E2E_DUMP=1 cargo test --test e2e         # save decoded frames to target/tmp/e2e
```

The end-to-end tests run `mer` on a pseudo-terminal that answers its capability probe like
Ghostty does, then decode the images and placeholder grids it writes.

The Inter font is licensed under the SIL Open Font License 1.1; see
`assets/fonts/LICENSE-Inter.txt`.
