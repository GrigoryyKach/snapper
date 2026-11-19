# Snapper

A lightweight region screenshot tool for Windows and Linux that lives in the tray. Press a hotkey, every monitor darkens, drag a rectangle,
draw on it if you like, and copy or save the result. One executable, no dependencies, works offline.

Written in Rust. The resident process is plain Win32 (hidden window, tray icon, global hotkey) and draws the capture overlay with
Direct2D, so it stays small and fast. The settings window is the same exe started with `--settings` and uses eframe/egui with the
NIMVORO suite look (`src/theme.rs`, `src/i18n.rs`, `src/nimvoro.rs`, copied from `../random_wheel` unchanged).

On Linux the same app is built from a second backend (`*_linux.rs`): the capture comes from X11 (`GetImage`, RandR) or the Wayland
screenshot portal, the overlay is an egui window (`Snapper --overlay`, started by the resident process), annotations are drawn
on the CPU with tiny-skia, the tray uses StatusNotifier/AppIndicator and the clipboard uses `arboard` (or `wl-copy` on Wayland).
The pure model code (`geom.rs`, `ann.rs`, `files.rs`, `hotkey.rs`, `config.rs`) is shared by both platforms.

## Platforms

- **Windows** 10/11 x64: `Snapper-win-x64.zip` from the release, or via the NIMVORO launcher.
- **Linux** x86_64, X11 and Wayland (built on Ubuntu 22.04, so glibc 2.35 or newer): `Snapper-linux-x64.tar.gz` from the release,
  or via the NIMVORO launcher.

### Linux

Unpack the archive and run `./snapper` (needs `libgtk-3-0`, `libxdo3` and, for the tray icon, `libayatana-appindicator3`; on GNOME also the AppIndicator extension).
The first start puts Snapper in the tray; `snapper --capture` takes a screenshot (it asks the running instance, or starts one).
Packages for a from-source build:

```bash
sudo apt install build-essential pkg-config libgtk-3-dev libayatana-appindicator3-dev libxdo-dev libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev
cargo build --release
```

**X11:** the screen is read with `GetImage`; the overlay is one borderless always-on-top window over all monitors; the global hotkey
(`PrintScreen` by default) works through `XGrabKey`.

**Wayland** (GNOME, KDE and others), known limitations:

- The compositor owns the screen, so Snapper asks the **xdg-desktop-portal Screenshot** portal for a picture of the whole desktop
  (GNOME may show a permission prompt the first time). The overlay is a fullscreen window on one output; with several monitors the
  stitched picture is fitted into that window.
- Apps cannot register global hotkeys on Wayland. Snapper tries the **GlobalShortcuts** portal (KDE, recent GNOME); where that is not
  available, bind a system shortcut to the command `snapper --capture`: GNOME *Settings > Keyboard > Custom Shortcuts*, KDE
  *System Settings > Shortcuts > Custom Shortcuts*. Settings shows the exact command with a Copy button.
- Clipboard: install `wl-clipboard` (`wl-copy`) for the most reliable copy on GNOME; without it Snapper falls back to the X11/data-control
  clipboard, which works on KDE and wlroots compositors.

## How it works

1. **Press the hotkey** (default `PrintScreen`) or, on Windows, left-click the tray icon. Every monitor is grabbed first and then shown dimmed,
   so the overlay never appears in the shot.
2. **Drag a rectangle.** It can span several monitors, or all of them. A label shows the size while you drag.
3. **Adjust it.** The selection is shown undimmed with eight handles (corners and middle of each side). With the **Move** tool
   (the default) you can drag the whole selection; drag a handle to resize.
4. **Annotate** with the toolbar that appears under the selection's bottom-right corner (it flips above or inside when there is no room,
   and always stays on one monitor).
5. **Copy or save.** `Ctrl+C` copies the image with its annotations to the clipboard and closes the overlay.

### Toolbar

| Button | What it does |
|---|---|
| Move | Drag the whole selection (default) |
| Pencil | Smoothed freehand line |
| Arrow | Line with a filled arrowhead |
| Rectangle | Outline |
| Filled rectangle | Solid fill |
| Text | Click, type (Unicode and IME work, Cyrillic included), `Enter` or a click elsewhere commits, `Shift+Enter` adds a line, `Esc` cancels only that text |
| Colour | 10 presets plus a HEX field; three stroke widths (also set the text size). The choice is remembered and applies to every tool |
| Undo | Multi-level (`Ctrl+Z`, redo with `Ctrl+Y`) |
| Scrolling capture | Scroll the selected area and stitch one tall image (not on Wayland) |
| Copy / Save / Close | `Ctrl+C` / `Ctrl+S` / `Esc` |

### Keys

| Key | Action |
|---|---|
| `Ctrl+C` or `Enter` | Copy the selection (with annotations) and close |
| `Ctrl+S` | Save to the screenshots folder and close |
| `Esc` | Close without anything (while editing text, only cancels the text) |
| `Ctrl+Z` / `Ctrl+Y` | Undo / redo |
| Right-click before a selection exists | Close |

Annotations live on the frozen image in screen coordinates: moving or resizing the selection afterwards does not move them.
The output is the selection crop at full physical resolution, the same pixels as on screen (per-monitor DPI aware, negative monitor
coordinates and mixed DPI work).

### Clipboard

On Windows the image is put on the clipboard as both `CF_DIB` and the registered `PNG` format, so Paint, Office, browsers, Discord and
Telegram all paste it. On Linux it is an `image/png` selection owned by the resident process (it must keep running, which X11 requires).
The data stays available after the overlay closes.

### Saving

Files are named `Snapper_YYYY-MM-DD_HH-mm-ss.png` (a numeric suffix is added on collision) and written to
`%USERPROFILE%\Pictures\Snapper` (`~/Pictures/Snapper` on Linux) unless you pick another folder in Settings. Format is PNG, or JPG at quality 90.
After saving a notification shows "Saved: ..."; clicking it opens Explorer with the file selected (the folder, on Linux).

## Scrolling capture

Captures a long page, or any scrollable content, as one tall image.

1. Start it from the **Scrolling capture** button in the overlay toolbar (after selecting an area), from the tray menu, or with the optional
   hotkey you set in Settings (empty by default). From the tray menu or the hotkey you then drag the area; it starts when you let go.
2. Select the scrollable part, for example the browser viewport. Leaving the page's scrollbar out gives the cleanest result.
3. Snapper grabs the area, sends mouse-wheel input to its right edge (so a link tooltip does not end up in the frames), waits until the
   content stops moving, and grabs again. A small bar outside the area shows the frames and the height with a **Stop** button; **Esc** stops too.
4. It stops at the end of the page, at the height limit (`scroll_max_height`, 30,000 px by default), on Esc or Stop, or when the frames stop
   matching (an animation or a video); in that case what was stitched so far is kept and a notice says so.
5. The result opens in a viewer with the usual tools, copy and save. It is shown 1:1 (fitted to the width if wider than the screen) and
   scrolls with the wheel, the arrow, Page Up/Down, Home and End keys, or by dragging with the Move tool. Copy and save use the full resolution.

**Stitching** (`src/stitch.rs`, covered by unit tests): each row of a frame is hashed (pixels lightly quantised, a possible scrollbar strip on
the right ignored) and the scroll distance is the shift at which most non-blank rows of two frames are equal; if no shift matches exactly (browsers
scroll by fractions of a pixel and redraw text a little differently) a tolerant comparison of per-row luma buckets is tried. Only the new strip
is appended. **Fixed headers and footers** (rows identical at the same position in both frames, such as sticky headers and cookie bars) are left out
of the search: the header stays once at the top, the footer of the last frame is added once at the bottom.

Limitations: only the page area you select is scrolled, so pages with their own scroll containers need the selection over that container;
floating elements that cover only part of the width (a "back to top" button, a chat bubble) are copied wherever they happen to be in a frame;
videos and animations stop the capture; the page's own scrollbar thumb is not stitched specially; windows of elevated programs ignore the
synthetic wheel input from a non-elevated Snapper. On Linux the feature is X11 only (XTest wheel events and `GetImage`): it is not available on Wayland, so the button, the tray item and the hotkey are hidden there. On Linux there is no Stop bar, only Esc.

## Tray

On Windows left click starts a capture and right click opens a menu (on Linux the menu opens on any click, as AppIndicator trays do): Take screenshot, Scrolling capture (X11 and Windows), Settings, Open screenshots folder, Updates
(check now, check on startup) and Exit. The menu follows the chosen theme.

## Settings

Right-click the tray icon, then **Settings** (or start `Snapper.exe` a second time). Changes apply immediately, no restart.

- **Capture:** hotkey recorder with a live status, the optional scrolling-capture hotkey, format (PNG/JPG), "Also copy to clipboard when saving".
- **Saving:** screenshots folder with Browse and Open.
- **System:** Start with Windows (a `HKCU\...\Run` value that reflects the real registry state), check for updates.
  On Linux the switch is "Start when you log in" (`~/.config/autostart/snapper.desktop`).
- **Interface:** language and theme.

### The Print Screen key on Windows 11

Windows 11 can reserve Print Screen for the Snipping Tool. If Snapper cannot register the hotkey, it shows a notification and an
inline warning in Settings. Either turn off **Settings > Accessibility > Keyboard > Use the Print screen key to open screen capture**,
or pick another hotkey. The recorder tries to register a new combination before accepting it and reports a clear error if it is taken.
Another program that already owns Print Screen (another screenshot tool, for example) causes the same warning.

## Languages

The interface is available in English (default), Russian and Ukrainian. Pick the language in Settings; it applies immediately,
including the tray menu, the toolbar tooltips and the notifications.
The starting language is taken from the `language` setting in `%APPDATA%\Snapper\config.json`, then from the `.nimvoro.json` marker
next to the exe (written by the NIMVORO launcher), and falls back to English. The result is saved to the config.

## Theme

Settings > Interface > Theme: System (default, follows the Windows setting live; on Linux it follows the portal or GNOME `color-scheme`, falling back to dark), Light or Dark.

## Files

- Config: `%APPDATA%\Snapper\config.json` on Windows, `~/.config/Snapper/config.json` on Linux (atomic writes, BOM-tolerant reads). Keys: `hotkey`, `autostart`, `save_dir`, `format`,
  `copy_on_save`, `language`, `theme`, `check_updates`, `last_color`, `last_stroke`, `scroll_hotkey` (empty by default), `scroll_max_height` (pixels, 30000).
- Status: `%APPDATA%\Snapper\status.json`: written by the resident process after each hotkey registration so Settings can show the result.

The settings process saves the config and then posts a registered window message (`Snapper.ReloadConfig`) to the resident process,
which reloads it and re-registers the hotkey. Only one resident process runs (a named mutex); starting `Snapper.exe` again opens Settings.
On Linux the same job is done by a Unix socket (`$XDG_RUNTIME_DIR/snapper.sock`, or `snapper.sock` in `SNAPPER_DATA_DIR`) that accepts
`reload`, `settings`, `capture` and `quit`; a second launch opens Settings, `--capture` takes a screenshot.

## Build

```bash
cargo run --release              # resident mode
cargo run --release -- --settings
cargo test --release
```

The ready exe is `target/release/Snapper.exe` (`target/release/Snapper` on Linux). `build.rs` embeds the icon, the version info and a Per-Monitor-V2 DPI manifest
(Windows SDK `rc.exe` on MSVC, `windres` on MinGW).
Set `SNAPPER_DATA_DIR` to keep the config somewhere else (handy for tests; such an instance also gets its own mutex, window class and messages on Windows, so it runs next to your normal Snapper) and `SNAPPER_LOG=1` to get a timing log in that folder.

## Release

A `vX.Y.Z` tag runs `.github/workflows/release.yml`, which tests and builds on Windows and on Ubuntu 22.04 and publishes `Snapper-win-x64.zip` and `Snapper-linux-x64.tar.gz`
in one release (the version in `Cargo.toml` must match the tag). The NIMVORO launcher installs and updates the tool from that release.

## Screenshots

See `assets/screenshots/`.

## Support

If you find this app useful, you can support its development: <https://donatello.to/MicsGame>
