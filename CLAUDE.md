# IDM2 (crate `idm`): context for continuing development

**IDM2** is the product name; use it everywhere users see a name (window title, sidebar, tray, extension, README, release zip). The exe and CLI command are `idm2` (`[[bin]]` in Cargo.toml); the crate name and the data folder (`%APPDATA%\idm`) keep `idm` so existing users' data carries over.

An IDM-style download manager for Windows in Rust: a segmented, resumable HTTP engine with a Slint GUI. See README.md for user-facing docs.

## Goals and preferences

- **Main goal: download speed equal to or better than IDM.** Engine performance comes before features.
- **Keep it minimal.** The owner prefers the smallest working change: stdlib first, no speculative abstractions, few files. Mark deliberate shortcuts with a `ponytail:` comment naming the limit and the upgrade path.
- **UI style is Notion.** Neutral greys, Segoe UI, 1px dividers, no heavy shadows, row actions revealed on hover, Tabler icons. Main progress bars are blue; per-connection bars are orange (`Theme.conn`).
- **Scope:** HTTP engine, BitTorrent (via librqbit), queue, global speed limit, browser integration (Chrome, Edge, Firefox), GUI, background running in the tray. A scheduler and categories were cut; ask before adding them.

## Layout

| File | Role |
| --- | --- |
| `src/engine.rs` | `probe`, `download`, `Limiter`, `SegView`. Contains the segment algorithm and the main engine test. |
| `src/manager.rs` | Queue (`max_active` = 3), `downloads.json` persistence, per-connection speed, `set_conns` / `set_limit`. |
| `src/torrent.rs` | `is_torrent`, `session`, `run` (librqbit). Peer stats. Contains the local-seeder test. See "Torrents" below. |
| `src/browser.rs` | axum on `127.0.0.1:47219`. `POST /add` accepts only `chrome-extension://` or `moz-extension://` origins. `POST /show` (single instance; optional body = magnet or .torrent path to add) accepts only requests *without* an `Origin`, so web pages can't trigger it. |
| `src/gui.rs` | Connects Slint to the manager; refreshes rows every 500 ms; `settings.json`; window controls; tray icon. See "Background running" below. |
| `src/main.rs` | No arguments opens the GUI; a single magnet link or `.torrent` path (how Windows launches us for those) opens the GUI with it; `idm2 <url> [-n N] [-o out]` runs the CLI. Release builds are GUI-subsystem; the CLI calls `AttachConsole`. |
| `ui/app.slint` | The whole UI: `Theme` global, `RowView`, `SettingRow`, `AppWindow`. `filter`: 0 = all, 1 = in progress, 2 = completed, 3 = settings page. |
| `ui/logo.svg` | The logo. `extension/icons/*.png` are rendered from it. |
| `extension/` | One MV3 extension for Chrome, Edge and Firefox. It catches `downloads.onCreated`, posts url, cookies, referrer and user agent, and cancels the browser's copy only if the app returns 200. The manifest lists both `background.service_worker` (Chromium) and `background.scripts` (Firefox), plus `browser_specific_settings.gecko` (id, min 121, `data_collection_permissions: none`). The JS uses `globalThis.browser ?? chrome`. |

## Torrents (don't break)

- **Routing.** `torrent::is_torrent(src)` is true for `magnet:` links and *local* `.torrent` paths. `Manager::run_inner` sends those to `run_torrent`; everything else takes the unchanged HTTP path. An http(s) link to a `.torrent` is downloaded like any file (so browser cookies work); when it finishes, `Manager::run` adds the saved file as a new torrent item.
- **One session, started lazily** (`Inner::torrents`, a `tokio::sync::OnceCell`) on the first torrent, so HTTP-only use never opens BitTorrent ports. DHT persistence is off, so librqbit writes nothing outside our folders. UPnP is on. The download speed limit is copied to `session.ratelimits` (applied separately from the HTTP limiter).
- **Never overwrite user files.** A fresh add uses `overwrite: false`, so librqbit fails if a file with that name exists. `Item.out` is recorded only once the torrent is `Live`, i.e. after librqbit accepted the files. Only then do later resumes pass `overwrite: true`. The test `downloads_from_a_local_seeder_and_never_overwrites_on_a_fresh_add` covers this.
- **Pause, stop, remove.** All three take the torrent out of the session (`session.delete`), which also ends seeding. Resume re-adds it and librqbit re-checks the existing files. Removing a *running* torrent passes `delete_files: true`, so librqbit deletes exactly its own files. Removing a *paused* torrent deletes `out` only if it's a single file; a multi-file torrent's folder is left alone, since it might hold the user's own files.
- **`out`** is the file itself for a single-file torrent, or the torrent's folder (librqbit's default sub-folder) for a multi-file torrent.
- **Peers in the UI.** `Item.peers` holds the 10 fastest live peers (speed is computed from `fetched_bytes` deltas) and `Item.conns` the live peer count. The GUI shows peers instead of connections when `is_torrent(url)`.
- **Registration.** The Settings button "Use IDM2" writes `HKCU\Software\Classes\magnet` and `.torrent` → `IDM2.torrent` via `reg.exe`, as `"<exe>" "%1"`. It has not been clicked during development because it changes the owner's file associations; ask before testing it on this machine.

## Background running

- **Event loop.** `ui.show()` + `slint::run_event_loop_until_quit()`. Hiding the window (close button, Alt+F4) doesn't end the loop; only the tray's **Quit** calls `quit_event_loop`.
- **Tray.** `tray-icon` with a muda menu: *Open IDM2*, separator, *Quit*. Left click opens the window. The tooltip shows the summary line. The icon pixels come from `extension/icons/48.png` (decoded with the `png` crate). Event handlers run off the UI thread, so they use `Weak::upgrade_in_event_loop`.
- **Single instance.** `gui::run` binds the port *before* creating the manager. If the bind fails and `POST /show` succeeds, a copy is already running, so this launch exits. Otherwise the second copy would run the same downloads twice.
- **Bringing the window forward.** `show_window` calls hide then show so the window comes to the front.
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
cargo test                 # 4 tests: HTTP engine, queue, browser endpoint (local axum server), torrent (local librqbit seeder on loopback)
cargo build --release      # target\release\idm2.exe (stop the running app first; the exe is locked)
```

- **Stuck build.** If a build sits at "Blocking waiting for file lock", VS Code's rust-analyzer `cargo check` may be hung. Kill that `cargo check` process.
- **Release zip.** Copy `idm2.exe` and `extension/` into `dist/IDM2-<ver>-windows-x64/` and zip it. Keep `Cargo.toml` and `extension/manifest.json` versions in sync. The zip is committed to git (the owner chose this over GitHub Releases); the staging folder is gitignored. Update the README download link to the new version.

## Testing the UI

1. Launch `target\release\idm2.exe` (or debug).
2. Run `tools\screenshot.ps1 -Out x.png [-ClickX -ClickY]` and look at the image.

Coordinates are physical pixels; at the owner's display scale, 1 logical px is about 2.25 physical px. Known positions in the default-size window:
- top row: (1300, 340)
- Settings nav item: (120, height − 30)
- All downloads nav item: (160, 160)
- New download button: (2051, 135)

Before typing with `SendKeys`, check that the foreground window belongs to `idm2`; otherwise the keystrokes go to another app.

For clean demo screenshots, start a second instance with `APPDATA` and `USERPROFILE` pointing at a temp folder, and pre-fill `downloads.json` there.

**Tray icon.** Windows 11 puts it in the hidden overflow (the **^** next to the clock). Clicking tray menus through screen automation is unreliable; ask the owner to check those by hand.

**Testing the Firefox extension end to end** (Firefox is installed; run while the app is running):

```powershell
npx.cmd -y web-ext@8 lint --source-dir extension      # expect 0 errors; 1 warning about service_worker is by design
npx.cmd -y web-ext@8 run --source-dir extension --no-reload --firefox "$env:ProgramFiles\Mozilla Firefox\firefox.exe" --start-url https://mirror.nforce.com/pub/speedtests/10mb.bin
```

Then check that `%APPDATA%\idm\downloads.json` gained the URL with a `User-Agent` header. Use `npx.cmd`, not `npx`: `Start-Process` can't launch `npx` directly. Killing `web-ext` leaves its Firefox open, so close the Firefox processes it started (check first whether the owner has their own Firefox open).

**Rendering SVGs:** use Edge headless (`msedge --headless --screenshot`). It caches `file://` images by name, so give each new SVG a new file name. Its minimum window is about 500 px, so crop the result.

## Benchmarking

- **Use** `https://mirror.nforce.com/pub/speedtests/100mb.bin` (or `1000mb.bin`). It allows many parallel ranges.
- **Avoid** `ash-speed.hetzner.com`: it drops more than about 3 parallel connections and rejects fake-Chrome user agents.
- **Avoid** `proof.ovh.net`: it rate-limited this IP.
- Run `curl` and `idm2` alternately; the line speed varies a lot.
- **Real torrent test:** send `https://releases.ubuntu.com/24.04/ubuntu-24.04.5-live-server-amd64.iso.torrent` to `/add` (with a `moz-extension://` Origin). That exercises the .torrent → auto-add path. It reached 3–4 MB/s from about 70 peers. Delete it afterwards: librqbit preallocates the full 3.8 GB.
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
- **Name:** the app was renamed from "Downloader" to **IDM2** at the owner's request (v0.2.0).
- **Release zips** are committed to the repo under `dist/` (the owner chose this over GitHub Releases).

## Backlog / known limitations

- No start with Windows (login autostart) yet.
- The tray menu (Open, Quit) has been tested only by its code path, not by clicking it.
- The save folder (`Downloads`) and simultaneous downloads (3) are hard-coded; they are candidates for the Settings page.
- `idm2.exe` has no embedded icon; it needs an `.ico` and a resource step in `build.rs`.
- Adaptive connections are not done yet: backing off to what a server accepts when it refuses extra connections.
- `Content-Disposition` `filename*=` (RFC 5987) isn't parsed, and URL-derived names aren't percent-decoded.
- The Firefox add-on isn't signed by Mozilla, so it loads only temporarily in regular Firefox. The exe isn't code-signed either.
- No real head-to-head speed comparison against IDM on the same file has been done yet.
- Torrents: no seeding option (it stops at completion, as the owner chose), no per-file selection, no upload limit, and the speed limit isn't shared with HTTP downloads (the combined total can reach 2x the limit). No fastresume, so resuming re-checks the files. A paused multi-file torrent's folder isn't deleted on remove.
