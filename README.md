# ⚠️ Project Moved / 项目已迁移

> **This project has been refactored and moved to a new repository. Please visit:**
>
> **👉 [https://github.com/jswysnemc/wayscrollshot](https://github.com/jswysnemc/wayscrollshot)**
>
> **本项目已重构并迁移至新仓库，请访问上方链接。**

---

# Long Shot RS (Archived)

[中文文档](README_CN.md)

High-performance native **long screenshot** (scrolling capture) application for **Linux Wayland**.

![Demo](https://img.shields.io/badge/Platform-Linux%20Wayland-blue)
![License](https://img.shields.io/badge/License-MIT-green)
![Status](https://img.shields.io/badge/Status-Archived-red)

## Features

- **Native Wayland Integration**: Uses `wlr-screencopy-unstable-v1` protocol directly
- **Real-time Stitching**: OpenCV-based image stitching with Sobel gradient + template matching
- **Low Latency**: Direct evdev input monitoring for scroll wheel detection
- **Always-on-top Preview**: Layer-shell overlay window that stays visible while scrolling
- **High Quality Export**: Full resolution PNG save or clipboard copy

## How It Works

### Architecture Overview

```
┌─────────────────┐     ┌──────────────────┐     ┌─────────────────┐
│  Thread A       │     │  Thread B        │     │  Thread C       │
│  Input Monitor  │────▶│  Worker          │────▶│  Overlay        │
│  (evdev)        │     │  (Capture+Stitch)│     │  (Layer Shell)  │
└─────────────────┘     └──────────────────┘     └─────────────────┘
         │                       │                       │
         ▼                       ▼                       ▼
   /dev/input/*           wlr-screencopy          Preview Window
   Scroll Events          Frame Capture           Save/Copy UI
```

### Module Description

| Module | File | Description |
|--------|------|-------------|
| **main** | `main.rs` | Entry point, thread orchestration |
| **input** | `input.rs` | Evdev scroll wheel monitoring |
| **capture** | `capture.rs` | Wayland screen capture via `wlr-screencopy` |
| **stitch** | `stitch.rs` | OpenCV image stitching algorithm |
| **worker** | `worker.rs` | Capture + stitch pipeline |
| **overlay** | `overlay.rs` | Layer-shell always-on-top preview window |
| **types** | `types.rs` | Shared data structures and channels |

### Stitching Algorithm

1. **Frame Capture**: Capture screen region via `zwlr_screencopy_manager_v1`
2. **Preprocessing**: Convert to grayscale, apply Sobel gradient (edge detection)
3. **Template Matching**: Extract template from current result's bottom, search in new frame
4. **Overlap Detection**: Find best match position with confidence threshold
5. **Content Append**: Append non-overlapping content to result image

Key parameters:
- Ignore top/bottom 15% (avoid fixed headers/footers)
- Template ratio: 20% of frame height
- Inertia constraint: max 500px search range (scroll direction aware)

### Layer Shell Overlay

Uses `wlr-layer-shell-unstable-v1` for always-on-top preview:
- Layer: `Overlay` (highest z-order)
- Anchored to screen edge, positioned next to selection
- Transparent background, custom button rendering
- Box-filter downscaling for clear preview

## Dependencies

### Build Dependencies

```bash
# Arch Linux
sudo pacman -S rust opencv clang cmake pkg-config

# Debian/Ubuntu
sudo apt install cargo rustc libopencv-dev libclang-dev cmake pkg-config

# Fedora
sudo dnf install rust cargo opencv-devel clang-devel cmake pkg-config
```

### Runtime Dependencies

```bash
# Arch Linux
sudo pacman -S opencv wl-clipboard

# Debian/Ubuntu
sudo apt install libopencv-core4.* libopencv-imgproc4.* wl-clipboard

# Fedora
sudo dnf install opencv wl-clipboard
```

### Wayland Compositor Requirements

Requires **wlroots-based** compositor supporting:
- `wlr-screencopy-unstable-v1` (screen capture)
- `wlr-layer-shell-unstable-v1` (overlay window)

**Supported**: Sway, Hyprland, river, wayfire, labwc, etc.

**Not Supported**: GNOME, KDE (use different protocols)

### Input Permissions

```bash
# Add user to input group (required for scroll detection)
sudo usermod -aG input $USER
# Log out and log back in
```

## Building

```bash
git clone https://github.com/yourname/long-shot-rs
cd long-shot-rs

# Release build (recommended)
cargo build --release

# Binary at: ./target/release/long-shot-rs
```

## Usage

```bash
./target/release/long-shot-rs [OPTIONS]
```

### Command Line Options

| Option | Description |
|--------|-------------|
| `-o, --output <PATH>` | Output file path (auto-generates timestamp-based name if not specified) |
| `-d, --save-dir <DIR>` | Output directory for auto-named files (default: ~/Pictures) |
| `-e, --exec <CMD>` | Command to execute after saving. Use `{}` as placeholder for file path |
| `-h, --help` | Print help information |
| `-V, --version` | Print version |

### Examples

```bash
# Basic usage - saves to ~/Pictures/longshot_YYYYMMDD_HHMMSS.png
./target/release/long-shot-rs

# Save to specific file
./target/release/long-shot-rs -o ~/screenshot.png

# Save to custom directory
./target/release/long-shot-rs -d ~/Screenshots/

# Open image after saving
./target/release/long-shot-rs -e "xdg-open {}"

# Open with specific viewer
./target/release/long-shot-rs -e "imv {}"

# Copy path to clipboard after saving
./target/release/long-shot-rs -e "echo {} | wl-copy"
```

### Workflow

1. **Select region**: Drag to select the scrollable area
2. **Scroll content**: Scroll slowly in the target window
3. **Preview**: Watch real-time stitching in the overlay
4. **Export**: Click Save (💾), Copy (📋), or Cancel (✕) button

### Tips

- Select only the scrollable content area, exclude fixed headers/toolbars
- Scroll slowly for better stitching accuracy
- Works best with text-heavy content (documents, web pages)

## Troubleshooting

| Problem | Solution |
|---------|----------|
| "No scroll devices found" | Add user to `input` group |
| "Compositor does not support..." | Use wlroots-based compositor |
| Poor stitching | Scroll slower, avoid blank areas |
| Preview not showing | Check if compositor supports layer-shell |
| Region selection not working | Check if compositor supports layer-shell |

## License

MIT

## Rust Dependencies

```toml
wayland-client = \"0.31\"          # Wayland protocol
wayland-protocols-wlr = \"0.3\"    # wlr extensions
smithay-client-toolkit = \"0.19\"  # Layer shell
opencv = \"0.93\"                  # Image processing
evdev = \"0.12\"                   # Input monitoring
arboard = \"3.4\"                  # Clipboard
crossbeam-channel = \"0.5\"        # Thread communication
```
