# glazeid

A minimal, extremely efficient workspace bar for [GlazeWM](https://github.com/glzr-io/glazewm).

Shows the active workspace and all available workspaces. Nothing else.

## Features

- One bar per monitor, anchored to any screen edge
- Active workspace highlighted with a filled pill
- Connects to GlazeWM over WebSocket and reacts to workspace events in real time
- Reconnects automatically if GlazeWM restarts
- Pure Rust — no WebView, no JS runtime, no system font dependency
- ~3 MB release binary (LTO + stripped)

## Requirements

- [GlazeWM](https://github.com/glzr-io/glazewm) running on the same machine
- Windows or macOS

## Installation

### From source

```sh
cargo install --path .
```

### From crates.io (once published)

```sh
cargo install glazeid
```

## Usage

Start glazeid after GlazeWM is running:

```sh
glazeid
```

glazeid will connect to GlazeWM on `127.0.0.1:6123` and create a bar on each monitor. It reconnects automatically if the connection drops.

Set `RUST_LOG=debug` for verbose output:

```sh
RUST_LOG=debug glazeid
```

## Configuration

glazeid looks for a config file at:

| Platform | Path |
|----------|------|
| Windows  | `%APPDATA%\glazeid\config.toml` |
| macOS    | `~/Library/Application Support/glazeid/config.toml` |

The file and its directory are created automatically with defaults if absent.

### All options

```toml
# Which edge of each monitor the bar attaches to.
# Values: "top" | "bottom" | "left" | "right"
position = "top"

# Height of the bar in logical pixels (or width when position is left/right).
bar_size = 28

# GlazeWM IPC port.
glazewm_port = 6123

# Milliseconds to wait before retrying a failed connection.
reconnect_delay_ms = 2000

# Bar background color.
background = "#1e1e2e"

# Text color for inactive workspaces.
foreground = "#cdd6f4"

# Fill color of the active workspace pill.
active_bg = "#89b4fa"

# Text color on the active workspace pill.
active_fg = "#1e1e2e"

# Font size in logical pixels.
font_size = 13.0

# Horizontal padding inside each workspace label.
label_padding_x = 10.0

# Vertical padding inside each workspace pill.
label_padding_y = 4.0

# Corner radius of the active workspace pill.
pill_radius = 4.0
```

Colors are specified as hex strings: `"#rrggbb"` or `"#rrggbbaa"`.

## How it works

| Layer | Technology |
|-------|------------|
| OS window | `winit` — one decoration-free, always-on-top window per monitor |
| Pixel buffer | `softbuffer` — CPU-mapped framebuffer, no GPU required |
| Drawing | `tiny_skia` — fills background, draws rounded-rect pills |
| Text | `fontdue` — rasterizes the embedded JetBrains Mono TTF |
| IPC | `tokio-tungstenite` — WebSocket client to GlazeWM on port 6123 |
| State | `tokio::sync::watch` — IPC task pushes updates; bar redraws only on change |

## License

Apache 2.0 — see [LICENSE](LICENSE).
