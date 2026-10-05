# Control protocol

`lightcraft --control 7980` (or `LIGHTCRAFT_CONTROL_PORT=7980`) starts a JSON-lines server on
`127.0.0.1:7980` (loopback only). One request per line, one reply per line, in order.

The first line on every connection must authenticate. A 64-hex token comes from
`--control-token-file` / `LIGHTCRAFT_CONTROL_TOKEN_FILE` (created mode `0600` on Unix if missing),
`--control-token` / `LIGHTCRAFT_CONTROL_TOKEN`, or — if you pass neither — one line on stderr for
that launch. Prefer a token file. See [SECURITY.md](../SECURITY.md).

```text
→ {"id": "auth", "method": "auth", "params": {"token": "<64 hex characters>"}}
← {"id": "auth", "ok": true, "result": {"authenticated": true}}
→ {"id": 1, "method": "engine.execute", "params": {"command": "photo.rate", "params": {"rating": 4}}}
← {"id": 1, "ok": true, "result": null}
← {"id": 2, "ok": false, "error": "unknown command `nope`"}
```

Anything else as the first line, including a wrong token, returns `authentication required` and
closes the connection. The listener accepts at most 16 connections, request lines up to 1 MiB and
replies up to 8 MiB.

Requests are answered on the UI thread between frames (timeout 60 s). The MCP server's connect
mode ([mcp.md](mcp.md)) is a thin layer over this channel. Implementation:
`crates/ui-egui/src/control.rs` (methods) and `apps/lightcraft/src/control_server.rs` (transport).

## Methods

| Method | Params | Result |
|---|---|---|
| `engine.execute` (alias `ui.menu.invoke`) | `{command, params?}` | Run any engine or UI command (see `engine.commands`) |
| `engine.commands` | — | Engine + UI commands: id, label, menu, shortcut, params doc, enabled |
| `ui.menu.list` | — | Menu entries (flat: id, label, menu path, shortcut, enabled) |
| `ui.menu.tree` | — | The menu bar as shown (File … Help): items `{id, params?, label, shortcut?, enabled, checked?}`, separators, submenus — the model behind the native macOS menu bar and the in-window menus |
| `ui.inspect` | — | UI state, window, canvas/image rects, `scroll: {grid, filmstrip}` (scroll offsets in points, `null` until drawn), active photo, selection, perf (`frameMs` = layout, `logicMs` = per-frame logic before it, `updateMs` = both, `maxUpdateMs`, `fps`, render queue …, `gpu` = adapter in use, `gpuReason` = why renders don't use the GPU, `gpuFallback` = latest render redone on the CPU and why — see `docs/gpu-pipeline.md`), status, memory (bytes per cache, see `library.memory`; plus stage caches and textures), `export: {running: {total, done, current} \| null, last}` |
| `ui.widgets` | `{filter?}` | On-screen widgets `{id, rect: [x, y, w, h]}` (screen points) |
| `ui.clickWidget` / `ui.dragWidget` / `ui.hoverWidget` | `{id, count?, fx?, fy?}` / `{id, toX?, toY?, dx?, dy?, steps?}` / `{id, fx?, fy?}` | Real egui input on a widget (hover: the pointer rests on it, e.g. for preset/profile previews) |
| `ui.move` / `ui.click` / `ui.drag` | `{x, y, count?, button?}` / `{x, y, toX, toY, steps?}` | Raw pointer input, screen points |
| `ui.pointer` | `{events: [{kind: down\|drag\|up, x, y}], alt?, shift?, cmd?}` | Gesture in normalized image coordinates (Detail view) |
| `ui.key` | `{key, cmd?, shift?, alt?, ctrl?}` | Key press |
| `ui.text` | `{text}` | Text input |
| `ui.scroll` | `{dx, dy}` | Mouse wheel |
| `ui.set` | partial UI state, e.g. `{"view": "detail"}` | Resulting UI state |
| `ui.dialog.confirm` / `ui.dialog.cancel` | — | Close the open dialog |
| `ui.resize` | `{width, height}` | Resize the window |
| `ui.screenshot` | `{path?, headless?}` | `{path, width, height}` once the frame (with finished renders) is captured. `headless: true` draws the UI on the CPU (no compositor needed); a windowed capture that gets no frame within 2 s falls back to headless automatically |
| `engine.execute {command: "app.export", params}` | export params (see `docs/mcp.md`), plus `preset`, `dir` / `path`, `ids`, `background` | Writes the files and returns `{files}`; with `background: true` (what the Export dialog and menus use) it returns `{background: true, total}` at once and the batch runs on a worker thread — poll `ui.inspect` → `export` |
| `ui.render` | `{id?, size?, path?}` | Render a photo (PNG to `path`), `{width, height}` |
| `app.quit` | — | Close the app |

### When the library can't be saved

With a persistent library, every command that changes something is written to the catalog journal (fsynced) before
it replies. If that write fails (disk full, volume gone, permissions), the command replies `ok: false` with
`"saved in memory but not written to disk: <reason>; LightCraft will retry"`. The change itself **is** applied (and
undoable) and stays queued: the next command, and the app's frame loop every couple of seconds, retry the write, so
nothing is lost once the disk is writable again — unless the app quits first. Meanwhile `ui.inspect` → `unsaved`
is `{ops, error}` (else `null`), `library.info` reports `unsavedOps` / `unsavedError`, and the top bar's cloud icon
shows a warning (widget `indicator:unsaved`). Queries and commands that change nothing still succeed. A failed
compaction (snapshot) is not a failed command — the log is kept whole — and only shows in `library.info` →
`lastError`.

## Headless rendering (no window, no GPU)

The egui UI can be rasterized on the CPU (`crates/ui-egui/src/softpaint.rs`, driven by
`crates/ui-egui/src/headless.rs`): same tessellation, gamma-space premultiplied blending and
scissor clipping as egui's GPU backends, so the image matches the window (minus GPU dithering).

- **In the running app:** `{"method": "ui.screenshot", "params": {"path": "a.png", "headless": true}}`.
  The UI is drawn into an offscreen context from the app's logic tick, which keeps running when the
  window is occluded or the display sleeps/locks. Windowed screenshots fall back to this after 2 s.
- **Without any app window:** `lightcraft-cli snapshot` runs a whole app session headlessly and
  answers the same control requests (same handler) from a JSON-lines script:

```text
lightcraft-cli snapshot --demo -o grid.png --size 1600x1000 [--scale 2]
lightcraft-cli snapshot --library DIR --script tour.jsonl -o shot.png
```

  `tour.jsonl` holds one request per line (`#` comments allowed), e.g.
  `{"method": "ui.set", "params": {"view": "detail", "right": "edit", "openSections": ["optics"]}}`
  then `{"method": "ui.screenshot"}`. Replies are printed to stdout. A `ui.screenshot` without
  `path` writes `-o` (then `OUT-2.png`, `OUT-3.png`, …); `ui.settle {timeoutMs?}` waits until no
  renders are in flight. Each request runs frames until it is answered and its injected input
  (clicks, keys, drags) has played out. Widget ids for `ui.clickWidget` come from `ui.widgets`
  (e.g. `button:upright-auto`). A 1600×1000 demo snapshot takes ~1–2 s (debug build).
