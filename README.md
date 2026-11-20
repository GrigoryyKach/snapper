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
| Scrolling capture (Windows) | Start a scrolling capture of the selected area |
| Copy text (Windows) | Copy all the text inside the selection (`Ctrl+Shift+C`), see below |
| Copy / Save / Close | `Ctrl+C` / `Ctrl+S` / `Esc` |

### Keys

| Key | Action |
|---|---|
| `Ctrl+C` or `Enter` | Copy the selection (with annotations) and close |
| `Ctrl+S` | Save to the screenshots folder and close |
| `Ctrl+Shift+C` | Copy the text inside the selection and close (Windows) |
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

## Scrolling capture (Windows only)

Captures a long page, or any scrollable content, as one tall image. **You scroll; Snapper watches and stitches.** The screen stays fully
usable the whole time.

1. Start it from the **Scrolling capture** button in the overlay toolbar (after selecting an area), from the tray menu, or with the optional
   hotkey you set in Settings (empty by default). From the tray or the hotkey you drag the area first, then press **Start** (or Enter).
2. The selection overlay closes. A thin frame with crop marks stays around the area (outside it, so it is never in the picture) and a small bar
   shows the live height and frame count, with **Done** and **Cancel**. The frame is click-through and the bar never takes focus: scroll, click
   and type in the page as usual, inside the area and outside it.
3. Scroll the page. Snapper samples the area every 70 ms, skips identical samples and appends new rows only from a stable view.
4. **Done** (the button, **Ctrl+Shift+Enter**, or the scrolling-capture hotkey pressed again) opens the result in the viewer; **Cancel** (the
   button or **Ctrl+Shift+Backspace**) discards it. Esc is not used, because you are busy browsing. If the page window closes, press Done to keep
   what was captured.
5. The result opens in the viewer with the usual tools, copy and save, at full resolution. It is shown 1:1 (fitted to the width if wider than the
   screen) and scrolls with the wheel, the arrow, Page Up/Down, Home and End keys, or by dragging with the Move tool.

**Stitching** (`src/manual.rs`, row signatures in `src/stitch.rs`, covered by unit tests): the canvas is a list of *pages* (segments) and a pointer
remembers where the last frame sat. Each frame is located near the pointer first by row hashes (pixels lightly quantised, a possible scrollbar strip
on the right ignored), with a tolerant luma comparison for fractional scrolling nearby; then anywhere in its page, then in the other pages, by exact
rows only.

- Below the captured rows: only the new strip is appended. Scrolling up, or back down over captured rows: the pointer moves and nothing is added,
  so nothing is duplicated.
- **Fixed headers and footers** (rows identical at the same place in consecutive frames, or equal to the top of the page) are left out of the
  comparison; the header stays once at the top, the footer of the last frame is added once at the bottom.
- **Pagination:** a frame that fits nowhere (you clicked "2") waits until the view is stable (about 0.4 s), then starts a new page. Leading rows
  that equal the top of an earlier page (the site header, the search box) are dropped, the page is joined where its own rows begin, and blank or
  half-loaded frames are never appended. Going back to an earlier page adds nothing; scrolling further down an unfinished page extends that page.
- Limits: `scroll_max_height` (30,000 px by default) and 40 pages. The result is cut there and a notice says so.

Limitations: only the selected area is watched, so pages with their own scroll containers need the selection over that container; scrolling
past the top of what was captured does not add rows above it; floating elements that cover only part of the width (a "back to top" button, a chat
bubble) are copied wherever they happen to be in a frame; video and animation inside the area are stitched as they look when stable; pages that
differ only slightly, or look alike, can be confused when the content is very repetitive; the page's own scrollbar column is not stitched specially.
Scrolling capture is **not available on Linux** (X11 or Wayland) in this version: the button, tray item, hotkey and settings row are absent there.

## Copy text (Windows only)

The **Copy text** button (`Ctrl+Shift+C`) puts every piece of text inside the selection on the clipboard as plain text (`CF_UNICODETEXT`)
and closes the overlay. A notice says how many lines were copied and how ("exact text" or "OCR"); with nothing found it says so and leaves
the clipboard alone. The work runs off the UI thread (a typical region takes about 0.1 to 0.3 s); a read that takes longer than a second shows
"Reading text...". The button, the key and this feature are absent on Linux.

Two sources read the same region at the same time (`src/textcopy.rs`):

1. **Exact text through UI Automation** (`src/uia.rs`): no recognition errors. Sample points inside the region find the elements under it.
   An element that offers a text pattern (browser pages, editors, documents) is read line by line from its visible ranges: lines inside the region
   whole, lines cut by the region word by word, lines that share a visual row (table cells) joined with tabs. Other elements (labels,
   buttons, list items, fields) give their name or value, laid out by their boxes. Password fields are never read. Chromium and Edge build their accessibility tree
   on the first request, so the very first read after a browser starts can take about half a second more.
2. **OCR with `Windows.Media.Ocr`** (`src/ocr.rs`, offline, no extra size) on the undimmed pixels, at native resolution:
   - the pixels become dark text on white whatever the background (inverted for light-on-dark, normalised against the surroundings for mixed
     areas) and the contrast is stretched;
   - upscaled 1 to 3 times with Lanczos so a text line is about 32 px tall, padded, and tiled at quiet rows when the engine's size limit is reached;
   - every engine for the user's profile languages and English, Russian and Ukrainian reads it (in parallel); the best is picked by script
     (whole Cyrillic words and a garbled Latin result mean Cyrillic; `ы э ъ ё` or `і ї є ґ` tell Russian from Ukrainian);
   - line bands the engine skipped (`a, b = 0, 1`) are cut out and read on their own;
   - look-alike letters are repaired inside a Latin line (`сап` to `can`, `0CR` to `OCR`);
   - the words are put back in reading order from their boxes: lines, paragraph breaks, indentation (in characters for monospace text),
     tab separated tables, prose columns read one after the other.

**Which text is used.** The exact text wins when it explains the OCR text: at least 80 percent of the OCR words with four letters or more are in it or
are damaged copies of its words (OCR slips letters, so a wrong word still counts). If OCR sees real words that the exact text lacks (an image of text next
to real text, a canvas), the exact text is only part of what is on screen and the OCR text is used, so half of the text is never returned silently. The exact text also
loses when it holds many words OCR finds hardly any of. With no exact text, the OCR text is used; with no OCR language, the exact text alone.

**OCR languages.** Windows installs OCR per language. If a language of your profile has none, and nothing is found, the notice says which one; add it in
Settings > Time & language > Language, with the "Optical character recognition" feature. Snapper installs nothing itself. Without the Ukrainian pack,
Ukrainian text read by OCR loses `і ї є ґ` (the Russian engine reads it); the exact text path is not affected.

**Accuracy** on a local test page rendered in Edge (character accuracy against the ground truth, whitespace ignored; 11 to 16 px, light and dark):

| Case | Exact text | OCR alone |
|---|---|---|
| English, Russian, mixed English and Russian paragraphs | 100% | 100%, 100%, 96% |
| Ukrainian (no Ukrainian OCR pack installed) | 100% | 69% to 72% |
| 11 px English, 12 px Russian | 100% | 100%, 98.6% |
| Light on dark, 11 and 14 px, English and Russian | 100% | 99.3% to 100% |
| Code block with indentation | 100% (layout exact) | 75% (the engine drops lines of short tokens) |
| Table | 100% | 98% |
| Confusable characters (`l1I\|`, `O0o`, `rn`/`m`) | 100% | 43% (the engine skips symbol lines) |
| Image of text (English and Russian, 13 and 22 px, light and dark) | none: only OCR can read it | 100% |
| Heading plus an image of text | 28% (heading only) | 99% (chosen) |

`cargo test --release` runs the unit tests (line, column and indentation rebuilding, the exact-versus-OCR choice, the engine pick, the
light-on-dark detection). The benchmark itself needs a visible test page and is run by hand:
`SNAPPER_BENCH_DIR=<dir> cargo test --release bench -- --ignored --nocapture` (see `src/textcopy.rs`).

## Tray

On Windows left click starts a capture and right click opens a menu (on Linux the menu opens on any click, as AppIndicator trays do): Take screenshot, Scrolling capture (Windows), Settings, Open screenshots folder, Updates
(check now, check on startup) and Exit. The menu follows the chosen theme.

## Settings

Right-click the tray icon, then **Settings** (or start `Snapper.exe` a second time). Changes apply immediately, no restart.

- **Capture:** hotkey recorder with a live status, the optional scrolling-capture hotkey (Windows), format (PNG/JPG), "Also copy to clipboard when saving".
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

## Updates

Without the launcher, the **Update** button updates the app by itself: it downloads the release archive from GitHub, checks its SHA-256 against the digest GitHub publishes for the asset, swaps the files (the old ones become `*.old`, are removed on the next start, and any failure rolls back) and restarts. If the app folder is not writable, the release page opens instead. Settings and the launcher language marker are never replaced.
