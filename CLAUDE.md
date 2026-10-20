# Downloader (crate `idm`): context for continuing development

An IDM-style download manager for Windows in Rust: a segmented, resumable HTTP engine with a Slint GUI. See README.md for user-facing docs.

## Goals and preferences

- **Main goal: download speed equal to or better than IDM.** Engine performance comes before features.
- **Keep it minimal.** The owner prefers the smallest working change: stdlib first, no speculative abstractions, few files. Mark deliberate shortcuts with a `ponytail:` comment naming the limit and the upgrade path.
- **UI style is Notion.** Neutral greys, Segoe UI, 1px dividers, no heavy shadows, row actions revealed on hover, Tabler icons. Main progress bars are blue; per-connection bars are orange (`Theme.conn`).
- **Scope:** engine, queue, global speed limit, browser integration, GUI. A scheduler, categories and a tray icon were cut; ask before adding them.

## Layout

| File | Role |
| --- | --- |
| `src/engine.rs` | `probe`, `download`, `Limiter`, `SegView`. Contains the segment algorithm and the main engine test. |
| `src/manager.rs` | Queue (`max_active` = 3), `downloads.json` persistence, per-connection speed, `set_conns` / `set_limit`. |
| `src/browser.rs` | axum `POST /add` on `127.0.0.1:47219`; accepts only `chrome-extension://` or `moz-extension://` origins. |
| `src/gui.rs` | Connects Slint to the manager; refreshes rows every 500 ms; `settings.json`; window controls. |
| `src/main.rs` | No arguments opens the GUI; `idm <url> [-n N] [-o out]` runs the CLI. Release builds are GUI-subsystem; the CLI calls `AttachConsole`. |
| `ui/app.slint` | The whole UI: `Theme` global, `RowView`, `SettingRow`, `AppWindow`. `filter`: 0 = all, 1 = in progress, 2 = completed, 3 = settings page. |
| `ui/logo.svg` | The logo. `extension/icons/*.png` are rendered from it. |
| `extension/` | MV3 extension: catches `downloads.onCreated`, posts url, cookies, referrer and user agent, and cancels the browser's copy only if the app returns 200. |
| `tools/screenshot.ps1` | Captures (and optionally clicks in) the running app window, for checking UI changes. |

## Engine invariants (don't break)

- **One file, no merge step.** `out.part` is preallocated with `set_len`; each connection writes at its own offset (`seek_write` / `write_all_at`). It becomes `out` by rename when done. `out.part.json` holds `{info, segs}` for resume.
- **Ranges tile the file.** A `Seg` is `{pos, end}`. A split cuts one range in two, so the ranges always cover the file end to end. A range's start is derived from that (`progress()`), never stored. A previous bug came from storing `start`.
- **Progress after write.** `pos` advances only after the bytes are written. `save()` snapshots the segments before calling `sync_data`, so saved state never claims unwritten bytes.
- **Dynamic segmentation.** `claim()` takes an unclaimed range, or else splits the biggest active one in half (`MIN_SPLIT` = 1 MiB). The owner of a split range sees `end` shrink and stops there. Bytes written past the new end are identical, so the overlap is harmless.
- **Live connection count.** `conns: Arc<AtomicUsize>`. The tick loop spawns workers only when the target goes *up*, so connections the server refused aren't retried every second. A lower target makes `claim()` return `None` once enough ranges are active.
- **HTTP/1.1 only** (`http1_only`). HTTP/2 would multiplex every range over one TCP connection.
- **Honest user agent** `idm/<version>`. Bot filters drop fake-Chrome clients, e.g. Hetzner. Browser downloads override it with the browser's own user agent.
- **Resume check.** Saved state is used only if `total` and `validator` (ETag or Last-Modified) still match; otherwise the download restarts from scratch.
- **Retries.** Up to 5 consecutive failures without progress, backing off 2 to 32 s. A worker that gives up releases its range for the others. The whole download fails only when no worker is left and bytes remain.
- **Speed limit.** One global `Limiter` (virtual scheduling) is awaited per chunk before writing. Not reading the socket applies TCP backpressure to the server.

## Commands

The Rust toolchain is installed but may not be on PATH in a fresh shell:

```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
cargo test                 # 3 tests, run against a local axum + tower-http ServeDir server
cargo build --release      # target\release\idm.exe (stop the running app first; the exe is locked)
```

- **Stuck build.** If a build sits at "Blocking waiting for file lock", VS Code's rust-analyzer `cargo check` may be hung. Kill that `cargo check` process.
- **Release zip.** Copy `idm.exe` and `extension/` into `dist/Downloader-<ver>-windows-x64/` and zip it. The zip is committed to git (the owner chose this over GitHub Releases); the staging folder is gitignored. Update the README download link to the new version.

## Testing the UI

1. Launch `target\release\idm.exe` (or debug).
2. Run `tools\screenshot.ps1 -Out x.png [-ClickX -ClickY]` and look at the image.

Coordinates are physical pixels; at the owner's display scale, 1 logical px is about 2.25 physical px. Known positions in the default-size window:
- top row: (1300, 340)
- Settings nav item: (120, height − 30)
- All downloads nav item: (160, 160)
- New download button: (2051, 135)

Before typing with `SendKeys`, check that the foreground window belongs to `idm`; otherwise the keystrokes go to another app.

For clean demo screenshots, start a second instance with `APPDATA` and `USERPROFILE` pointing at a temp folder, and pre-fill `downloads.json` there.

**Rendering SVGs:** use Edge headless (`msedge --headless --screenshot`). It caches `file://` images by name, so give each new SVG a new file name. Its minimum window is about 500 px, so crop the result.

## Benchmarking

- **Use** `https://mirror.nforce.com/pub/speedtests/100mb.bin` (or `1000mb.bin`). It allows many parallel ranges.
- **Avoid** `ash-speed.hetzner.com`: it drops more than about 3 parallel connections and rejects fake-Chrome user agents.
- **Avoid** `proof.ovh.net`: it rate-limited this IP.
- Run `curl` and `idm` alternately; the line speed varies a lot.
- Results so far: 16 connections ran about 3.5× a single `curl`; one demo run reached 28 MB/s with 8 connections.

## Slint pitfalls hit so far

- **Reserved names.** Don't name properties or functions `row`, `color` (on a Rectangle), `focus` or `close`; they clash with built-ins.
- **Deprecated names.** Use `content-height` instead of `viewport-height`, and `transform-rotation` instead of `rotation-angle`.
- **No `visible` on layouts.** Wrap the layout in a `Rectangle` and set `visible` on that.
- **Hover with nested buttons.** Put the layout *inside* the row's `TouchArea`, so hovering a child button keeps the row's `has-hover`.
- **Updating rows.** Refresh rows with `set_row_data` when the count is unchanged. `set_vec` recreates the delegates and loses hover and expanded state.
- **Frameless window.** `no-frame: true`. Dragging works through `drag(dx, dy)` → `window.set_position` (multiply by `scale_factor`); edge resizing still works.
- **Icons.** Tabler SVGs (stroke `currentColor`) are recoloured with `colorize`. Fetch them from `https://unpkg.com/@tabler/icons@3.24.0/icons/outline/<name>.svg`.

## Decision history

- **Logo:** "Slices + golden tip": Fibonacci-width bars (34/21/13) fading into a golden-gnomon arrowhead (108° apex), on a blue gradient tile.
  - Rejected: an IDM-style globe; a Φ-arrow (looked like ♂/♀); a funnel (reads as a filter icon); a shield; a Ψ shape.
  - Lesson: abstract circle-and-line marks get read as other symbols, so keep "download" unambiguous.
- **Per-connection view:** the file-map bar was removed at the owner's request. Only per-connection rows remain, and their numbers are relative to each connection's own range.
- **Settings page:** holds connections per download (1–32, default 8), the speed limit (MB/s), and browser status.

## Backlog / known limitations

- No tray icon. Closing the window stops downloads, which resume on the next launch.
- The save folder (`Downloads`) and simultaneous downloads (3) are hard-coded; they are candidates for the Settings page.
- `idm.exe` has no embedded icon; it needs an `.ico` and a resource step in `build.rs`.
- Adaptive connections are not done yet: backing off to what a server accepts when it refuses extra connections.
- `Content-Disposition` `filename*=` (RFC 5987) isn't parsed, and URL-derived names aren't percent-decoded.
- Only Chrome and Edge are supported (no Firefox build), and the exe isn't code-signed.
- No real head-to-head speed comparison against IDM on the same file has been done yet.
