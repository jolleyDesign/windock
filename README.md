# Windock

A clean, fast dock for Windows 10/11, in the spirit of the macOS Dock and GNOME's dash.

- Pinned apps plus running apps, with indicators for open windows, the focused app and apps asking for attention
- Live window previews on hover, like the Windows taskbar
- macOS-style magnification (optional), frosted-glass background, light/dark theme that follows Windows
- Bottom, top, left or right edge, any monitor, optional auto-hide
- Can hide the Windows taskbar while it runs, and puts it back when it quits
- Drag icons to reorder, drag an icon off to remove it; drop files onto an app to open them with it, or between apps to pin them
- Keyboard shortcuts for pinned apps and for showing the dock
- Hot-reloaded TOML config
- Native Rust + Win32: uses no CPU when idle, ~25 MB RAM, one ~700 KB `.exe`, no runtime to install

## Using it

| Action | Result |
| --- | --- |
| Click an app that isn't running | Launch it (the icon bounces until a window appears) |
| Click a running app | Bring its most recent window forward |
| Click the focused app | Minimize it (or cycle through its windows, if it has several) |
| Middle-click | Open a new window / instance |
| Hover a running app | Live previews of its windows: click one to switch to it, **✕** or middle-click to close it |
| Right-click | Pick a window (when there are several), Open, New window, Keep in dock / Remove from dock, Close windows, Import taskbar pins, Dock settings…, Quit |
| Drag an icon along the dock | Reorder pinned apps |
| Drag an icon off the dock and release | Unpin it |
| Drop files onto an app | Open them with that app |
| Drop files, shortcuts or folders between apps | Pin them at that position (programs and shortcuts always pin) |
| Ctrl+Alt+1–9 (0 for the 10th) | Open or switch to that pinned app, like clicking it |
| Win+Shift+D | Show or hide the dock (with auto-hide: slide it in) |

## Configuration

`%APPDATA%\Windock\config.toml` is created on first run with every option commented. Right-click
the dock → **Dock settings…** opens it. Changes apply as soon as you save. If the file has an
error, the dock shows it and keeps the last good settings.

```toml
[dock]
position = "bottom"      # bottom | top | left | right
monitor = "primary"      # or 0, 1, ...
autohide = false
reserve_space = false    # keep maximized windows from covering the dock
hide_taskbar = false     # hide the Windows taskbar, clock and tray icons included
show_running = true
show_all_desktops = false
show_labels = true
previews = true          # live window previews on hover
hide_on_fullscreen = true
click_minimizes = true
autostart = false        # start at sign-in
gpu = false              # GPU rendering: ~40 MB more RAM; software is plenty for a dock

[appearance]
theme = "auto"           # auto | dark | light
icon_size = 48
spacing = 6
padding_x = 8            # left and right of the icons
padding_y = 8            # above and below the icons
edge_margin = 8
corner_radius = 18
blur = true
indicator = "dot"        # dot | line | none
# background = "#1c1c1e8c" (and border, indicator_color, active_indicator_color,
# attention_color, label_background, label_text): "#RRGGBB" or "#RRGGBBAA"

[magnification]
enabled = true
scale = 1.6
range = 3.0

[animation]
enabled = true
speed = 1.0
bounce_on_launch = true

[shortcuts]
open_app = "ctrl+alt"    # + 1-9 opens pinned app N; "" turns it off ("win+ctrl+alt" on AltGr keyboards)
toggle_dock = "win+shift+d"

[[pinned]]
path = 'C:\Windows\explorer.exe'          # .exe, .lnk, folder, or
name = "File Explorer"                    # 'shell:AppsFolder\<AppUserModelID>' for Store apps
# args = "..."  working_dir = "..."  (optional; %ENV_VARS% are expanded)
```

To find a Store app's ID, run `Get-StartApps` in PowerShell.

The log is at `%APPDATA%\Windock\windock.log`.

## Building

The build cross-compiles from WSL (or Linux) using [`cargo-xwin`](https://github.com/rust-cross/cargo-xwin):

```sh
rustup target add x86_64-pc-windows-msvc
cargo install cargo-xwin --locked
scripts/deploy.sh          # build, install to %LOCALAPPDATA%\Programs\Windock, (re)start
scripts/deploy.sh --stop   # stop the running dock
```

On Windows itself, `cargo build --release` works with the MSVC toolchain.

## How it works

| File | Role |
| --- | --- |
| `src/app.rs` | The dock window: layout, magnification, input, drag & drop, auto-hide, animation loop |
| `src/app/preview.rs` | The hover popup with live DWM window thumbnails |
| `src/app/droptarget.rs` | Files dragged onto the dock: open with an app, or pin |
| `src/hotkeys.rs` | Parsing shortcut strings like `win+shift+d` |
| `src/settings.rs` | The settings window (`windock.exe --settings`, egui) |
| `src/softgui.rs`, `src/softgui/raster.rs` | Hosts the settings window and draws it on the CPU, so it needs no GPU driver (~10 MB instead of ~170 MB) |
| `src/taskbar.rs` | Reads the taskbar's pinned apps for importing, and hides the taskbar |
| `src/logo.rs`, `build.rs` | The app icon, drawn in code and embedded at build time |
| `src/apps.rs` | Finds taskbar-worthy windows and groups them by AppUserModelID / executable |
| `src/render.rs` | A shared Direct2D device plus a composition surface per window, each with a host-backdrop (blur) visual |
| `src/icons.rs` | Loads high-resolution shell icons on a background thread |
| `src/config.rs` | TOML schema, theme palettes, comment-preserving saves of `[[pinned]]` |
| `src/shell.rs` | Launching, shortcuts, version info, registry, theme detection |

How the pieces fit:

- **Idle:** the UI thread sleeps in `GetMessage`.
- **Animating:** it renders once per display refresh, paced by `DwmFlush`, and redraws only the area that changed.
- **Window tracking:** WinEvent hooks and the shell hook report window changes, so there is no polling.
- **Click-through:** the window region tracks the visible dock, so the transparent parts pass clicks to whatever is below.
