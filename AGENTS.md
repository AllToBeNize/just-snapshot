# Repository Rules

Guide for AI coding agents working on Snapshot. The user-facing feature overview lives in `README.md`; the architecture rationale lives in `docs/technical-plan.md`.

## Project Overview

Snapshot is a Windows 10/11 x64 screenshot tool written in Rust on raw Win32 (`windows` / `windows-sys` crates, GDI drawing, no UI framework). It idles in the tray with a blocking message loop; every capture buffer, GPU resource, and helper thread is created per capture session and released afterwards. Keep idle CPU, wake-ups, and memory at zero growth.

UI strings, error messages, and user-facing docs are Simplified Chinese. Code, identifiers, and comments are English.

## Build and Test

Build and test on Windows only. `snapshot.ps1` locates Cargo and the Visual Studio developer environment itself.

| Command | Purpose |
| --- | --- |
| `.\snapshot.ps1 build` | Release build: `target/x86_64-pc-windows-msvc/release/snapshot.exe` |
| `.\snapshot.ps1 test` | Run all unit tests |
| `.\snapshot.ps1 package` | Test, build, then write `dist/snapshot-v<version>-windows-x64.zip` and `.sha256` |
| `.\snapshot.ps1 release -Tag vX.Y.Z` | Package and create a draft GitHub Release (needs `gh auth login`) |

- The script writes rustup progress to stderr. In a remote or non-interactive PowerShell session that is turned into a terminating error; run it from `cmd` instead (`powershell -NoProfile -ExecutionPolicy Bypass -File snapshot.ps1 test > log 2>&1`).
- Run `.\snapshot.ps1 test` before reporting a change as done. The build must stay warning-free.
- A running `snapshot.exe` locks the release binary; exit it from the tray before rebuilding.
- UI previews without a desktop session: run the test binary in `target/x86_64-pc-windows-msvc/release/deps/snapshot-*.exe` with `preview --ignored`. It renders the settings window (every theme and main states), the toolbar and option bar in every theme, the dialog, the text frame and the tray icons to `%TEMP%\snapshot-preview\*.png`. Look at them after any UI change.

## Release Flow

1. Bump `version` in `Cargo.toml`, run `.\snapshot.ps1 build` to sync `Cargo.lock`, and commit.
2. `git tag vX.Y.Z`, `git push origin HEAD`, `git push origin vX.Y.Z`.
3. `.\snapshot.ps1 release -Tag vX.Y.Z` creates a **draft** with the ZIP, SHA256, and generated notes. It refuses to run on a dirty tree, when the remote tag is not at `HEAD`, or when a release for the tag already exists.
4. A human reviews and publishes the draft on GitHub. Do not tag, push, or publish unless asked.

## Code Map

| Module | Responsibility |
| --- | --- |
| `main.rs` | Tray icon and menu, global hotkey registration, first-run setup, applying settings |
| `hotkey.rs` | Binding list, defaults (capture `Ctrl+Alt+A`), parsing, `shortcuts.txt` load/save, `first_run()` |
| `settings.rs` | Settings window: frameless and custom-drawn (GDI + GDI+), DPI-scaled, key recording, live hotkey probe, theme picker |
| `app_icon.rs` | Tray icon drawn in code, with an amber warning badge variant |
| `startup.rs` | Start-at-login value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\Snapshot` |
| `capture.rs` | Desktop grab, cropping, the `Surface` bitmap helper and PNG encoding |
| `output.rs` | The only exit for captures: `deliver` copies (opaque bottom-up DIB + PNG), saves or pins. Normal and long captures both use it |
| `selection/mod.rs` | The capture session: state, window creation and message dispatch |
| `selection/chrome.rs`, `annotate.rs`, `text.rs`, `pointer.rs`, `paint.rs`, `export.rs` | Toolbar/option/hint state and commands; strokes; text editing; dragging, snapping, loupe and cursors; painting; leaving the overlay |
| `selection_geometry.rs`, `targets.rs`, `panel_edges.rs` | Rect math, window/UIA target snapping, content-column edge detection |
| `toolbar.rs` | Main toolbar layout, painting, hit testing; shared `paint_panel` / `paint_button` |
| `annotation_options.rs` | Secondary option bar (width, font size, colors) |
| `annotation.rs`, `text_mark.rs`, `text_render.rs`, `mosaic.rs` | Annotation model and history, text boxes, capturing the hidden text editor, mosaic |
| `gdiplus.rs` | GDI+ wrapper: anti-aliased shapes, vector icon rasterizing, image drawing |
| `overlay_ui.rs`, `typography.rs` | Hint panel, fonts |
| `icons.rs`, `icons/lucide.rs` | Vector icons rendered at the exact target size. `icons/lucide.rs` is generated: edit SVGs in `tools/icons/` and run `python tools/svg2rust.py tools/icons <lucide version> <names...> > src/icons/lucide.rs` |
| `theme.rs` | Color themes (Graphite default, Midnight, Forest, Light) and color helpers |
| `ui.rs`, `dialog.rs` | Shared drawing helpers for custom-drawn windows; the themed dialog that replaces `MessageBox` |
| `scroll_capture.rs`, `scroll_input.rs`, `scroll_stitch.rs` | Long capture session, wheel forwarding, frame stitching |
| `pinned.rs` | Pinned image windows |

## UI Conventions

The overlay follows Snipaste's layout:

- The main toolbar is centered directly below the selection. If it does not fit, it goes above; if neither fits (a full-screen selection), it goes inside the selection's bottom edge.
- Toolbar groups: drawing tools | undo, redo | long capture, pin, save, exit, copy. `GROUP_STARTS` in `toolbar.rs` defines the dividers.
- The secondary option bar is centered under the toolbar. It opens downward when the toolbar is below the selection, otherwise upward, so it never covers the selection. It does not follow the text box.
- Panels take their colors from `theme::palette()` and are drawn anti-aliased with GDI+, with a 2 px shadow inside their bounds (`toolbar::SHADOW`). Never hard-code colors in UI code; add a palette field instead.
- Text boxes use Snipaste-style chrome: a 1 px dashed outline, small square corner handles and a rotation knob. Anything painted must stay within the rect that gets invalidated, or it leaves trails.
- Layout changes need matching updates to the unit tests in `toolbar.rs`, `annotation_options.rs`, and `selection/tests/text.rs`.
- Clipboard behavior has an on-demand test against the real clipboard; run it in a desktop session after touching `output.rs`: `<test binary> clipboard --ignored --test-threads=1`.

## Defaults and Local State

| Location | Content |
| --- | --- |
| `%LOCALAPPDATA%\Snapshot\shortcuts.txt` | Shortcuts and pin opacity step. Its absence (together with no legacy `hotkey.txt`) marks the first run, which enables start at login |
| `%LOCALAPPDATA%\Snapshot\hotkey.txt` | Legacy capture shortcut, migrated on first run |
| `HKCU\...\Run\Snapshot` | Start-at-login command, pointing at the running executable |

A capture hotkey conflict must never stop the app or open a modal dialog. The tray icon switches to the warning badge, the tooltip names the conflict, one non-modal notification is shown (clicking it opens settings), and the tray menu gets a "change shortcut" item. Registration is retried whenever the tray menu or settings opens. Saving an occupied shortcut is allowed; the settings window shows the conflict live before saving.

## Settings Window

- Everything is painted in `Painter`; `Layout` owns every rect so painting and hit testing agree. Add new controls to `Layout::targets`.
- Sizes are 96-DPI constants multiplied by the window's DPI scale; fonts are recreated on `WM_DPICHANGED`.
- Animations run on one 15 ms timer that stops when everything settles. Do not add idle timers.
- Colors: accent for primary actions, amber for warnings (conflicts, duplicates), red only for errors. The window is frameless: the header is the drag area and has its own close button.
- Errors go through `show_error`, which opens the themed dialog in `dialog.rs`; do not call `MessageBoxW`.

## Text Editing and Change Visibility

- Use `apply_patch` for ordinary text and source-code edits. Preserve the original encoding and line endings. If `apply_patch` cannot safely preserve them, use a narrowly scoped alternative and explain why.
- If no change card is visible, identify the modified files and summarize the changes in the final response.

## Temporary Files

- Keep temporary AI scripts, probes, screenshots, and verification output outside the repository, under a task-specific directory in the system temporary directory.
- Exclude `AI_Temp/` and temporary artifacts from staging, commits, and submission lists.

## File Deletion

- When deleting files or directories with PowerShell, do not pass `-Force` to `Remove-Item`.
