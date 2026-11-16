# Docker Desktop-equivalent Linux GUI (Rust + Tauri) design

> **Status: Superseded / historical.** This is the rev.2 design (2026-11-10). It no longer describes the current code.
>
> - Since 2026-11-12 the UI is rendered natively with [noa-gpui](https://github.com/simota/noa-gpui), now consumed as `noa-gpui-platform` from crates.io. Tauri, React, the WebView and xterm.js are gone.
> - `src-tauri/` became `app/` (binary `docker-stack`). `ui/` and the whole npm setup were removed.
> - The command layer (`app/src/commands.rs`) and state management (`app/src/state.rs`) remain. IPC was replaced by channels in `app/src/backend.rs`, through which state changes reach the UI. The UI can only run the fixed functions in `app/src/commands.rs`.
> - Exec uses noa-gpui's Terminal widget.
> - Distribution is `scripts/install.sh`, which builds from source on Linux and macOS. No `.deb`, `.rpm` or AppImage is produced.
> - The sections on the UI, IPC (§4), the frontend (§6), Tauri security (§7), build and distribution (§9) and E2E (§10) describe the old design.
> - For the current architecture, see [README.md](../../README.md), [AGENTS.md](../../AGENTS.md) and [docs/feature-map.md](../feature-map.md).
>
> Original rev.2 header:
>
> - Status (rev.2, 2026-11-10): replaced the UI layer of rev.1 (gpui) with Tauri. M0 to M6 implemented (verification results in §10).
> - Target (rev.2): Linux (X11 / Wayland), x86_64 / aarch64
> - Technology (rev.2): Rust stable, `tauri` 2.12.x (3.x is alpha, so not adopted), `bollard` 0.21.x, `tokio` 1.x
>   - Frontend: TypeScript + React + Vite, `@tanstack/react-virtual`, `@xterm/xterm` 6.x (see open questions in §12)

## 0. Changes from rev.1 (gpui)

| Item | rev.1 (gpui) | rev.2 (Tauri) |
|---|---|---|
| UI | Native rendering with gpui + gpui-component | React on a WebView (WebKitGTK on Linux) |
| Bridging async runtimes | Needs a custom bridge between gpui's executor and tokio | Not needed. Tauri async commands run on tokio |
| Log ANSI parsing and line buffer | Rust side (`logbuf.rs`) | Frontend side. Rust only sends raw text in batches |
| exec terminal | Terminal implemented with `alacritty_terminal`, drawn on a canvas by hand | xterm.js. Rust only relays bytes |
| Distribution | Custom packaging | Tauri bundler produces `.deb` / `.rpm` / AppImage |

`crates/engine` (bollard wrapper, error classification, target resolution, compose CLI invocation, exec) is used unchanged. The 4 live tests from the rev.1 work pass against a real dockerd (`DOCKER_STACK_LIVE=1 cargo test -p engine --test live`).

`crates/app` (the gpui UI) has been deleted. M0 ported `config.rs`, and the connection state transitions and backoff from `store.rs`, to `src-tauri`. M1 ported `ui/rows.rs` (compose grouping) and `logbuf.rs` (ANSI parsing), with their test cases, to `ui/src/lib/rows.ts` and `ui/src/lib/ansi.ts`.

## 1. Decisions (unchanged from rev.1)

Build it as **a GUI client that connects over the Docker Engine API to a Docker Engine (dockerd) already running on the host**.
It does not manage a VM or bundle dockerd. Only compose operations call the `docker compose` CLI.

The assumption that would break this design: React on WebKitGTK can follow 10,000 log lines per second and render xterm.js at a usable speed.
M1 measured it: at about 10,000 lines per second, the longest main-thread stall was 9-12ms (§10). The alternative in §6 (a ring buffer on the Rust side) was therefore not adopted.

## 2. Scope

### In scope
Same feature list as rev.1.
- Connection management: context, `DOCKER_HOST`, rootless, guidance on permission errors, automatic reconnect
- Containers: list, start / stop / restart / kill / pause / unpause, remove, logs, inspect, stats, exec
- Images: list, pull (with progress), remove, prune dangling
- Volumes / networks: list, create, remove, prune
- Compose: grouping by label, up / down / restart / stop / start / pull
- Live updates from events, confirmation dialogs, toast notifications, keyboard shortcuts, light/dark theme, settings screen

### Out of scope
Bundling a VM / dockerd, Kubernetes, Extensions, Docker Hub / Scout integration, a BuildKit build UI, Dev Environments, a file browser, Windows / macOS, Flatpak.
Tauri could provide a system tray, but it is not part of the MVP.

## 3. Architecture

```
┌──────────── WebView (React) ─────────────┐      ┌──────────── Rust (tokio) ──────────────┐
│ views ── store(Zustand) ◀── event ───────┼──────│ AppState { engine, conn, generation }  │
│   │ invoke("container_action", …)        │      │   events loop → "state-changed"        │
│   ▼                                      │ IPC  │ #[tauri::command] async fn …           │
│ api.ts (typed invoke wrapper) ───────────┼─────▶│   └─ crates/engine (bollard)           │
│ LogPane ◀─ Channel<LogBatch> ────────────┼──────│ logs / stats / pull / exec / compose   │
│ Terminal(xterm.js) ⇄ Channel / invoke ───┼──────│   are streamed over Channel            │
└──────────────────────────────────────────┘      └────────────────────────────────────────┘
```

- The Rust side is the source of truth for state: connection state, generation number, the set of in-flight operations (pending), and pull and compose progress.
  - The frontend is only a cache that receives and displays that state.
  - Reloading the WebView recovers everything by fetching the full state again with `get_snapshot`.
- List data is synced as in rev.1: fetch everything right after connecting and on reconnect, then treat events as notifications of "which kind of resource changed". Changes are coalesced with a 100ms debounce, and the lists are fetched again.
  - On every state change, the whole snapshot is sent in a `state-changed` event, throttled to at most once per 50ms.
  - The plan started with per-kind events (`resources-changed`, `pull-progress`, etc.), but the implementation uses a single event. That avoids diff consistency issues and keeps ordering by `revision` in one place. Even with thousands of items, each send is a few hundred KB.

### Directory layout

```
docker-stack/
├─ Cargo.toml              # workspace: crates/engine, src-tauri
├─ package.json            # npm scripts and dependencies. vite.config.ts points root at ui/
├─ crates/engine/          # unchanged
├─ scripts/ui-eval.mjs     # verification script that evaluates JS in the running WebView (§10)
├─ scripts/e2e.mjs         # E2E test against the real app + real dockerd (§10)
├─ src-tauri/              # Tauri app (Rust)
│  ├─ build.rs             # lists the exposed commands in AppManifest
│  ├─ src/main.rs          # Builder, invoke_handler registration, type definition generation test
│  ├─ src/dto.rs           # types that cross IPC (engine types never cross)
│  ├─ src/state.rs         # AppState, connection state transitions, events loop and debounce, operations, pull, compose
│  ├─ src/commands.rs      # list of #[tauri::command] (§4)
│  ├─ src/streams.rs       # Channel sends for logs/stats/exec, batching, cancellation
│  ├─ src/config.rs        # reading and writing settings
│  └─ capabilities/default.json
├─ ui/                     # React + Vite
│  ├─ src/bindings.ts      # generated (tauri-specta). Do not edit by hand
│  ├─ src/api.ts           # typed wrappers for invoke and Channel (invoke is called nowhere else)
│  ├─ src/conn.ts          # pure functions that turn connection state into display values
│  ├─ src/store.ts         # Zustand: snapshot and event handling
│  ├─ src/pages/{Containers,Images,Volumes,Networks,Settings}.tsx
│  ├─ src/components/{ContainerDetail,LogPane,Terminal,StatsPane,InspectPane,Overlays,ui}.tsx
│  └─ src/lib/{rows.ts,ansi.ts,format.ts}   # pure functions, tested with vitest
└─ docs/plans/
```

## 4. IPC contract (boundary between Rust and TypeScript)

The frontend can call only the fixed commands below; no generic proxy passes Docker API calls through.
Access to docker.sock is effectively root, so a compromised WebView can do no more than this list allows.

| command | arguments | return value / mechanism |
|---|---|---|
| `get_snapshot` | — | `Snapshot { revision, conn, target, containers, images, volumes, networks, pending, pulls, compose_runs, compose_available }` |
| `retry_now` | — | `()`. Skips the wait and reconnects immediately |
| `refresh` | — | `()`. Fetches every kind of list again |
| `list_contexts` | — | `ContextEntry[]` |
| `use_context` | `name` | `()`. Saves it to the settings and reconnects |
| `container_action` | `id, action: {action: start\|stop\|restart\|kill\|pause\|unpause} \| {action: remove, force, volumes}` | `Result<(), UiError>`. Completion and failure are reported through state changes and `notice` |
| `inspect` | `kind: container\|image, id` | Formatted JSON string |
| `remove_image` / `remove_volume` / `remove_network` | `id, force` / `name` / `id` | `Result<(), UiError>` |
| `create_volume` / `create_network` | `name` (validated on the Rust side against `[A-Za-z0-9][A-Za-z0-9_.-]*`) | `Result<(), UiError>` |
| `prune` | `kind` | `Result<(), UiError>`. The count and reclaimed space are reported through `notice` |
| `pull` | `reference` | `Result<(), UiError>`. Progress goes into `Snapshot.pulls` |
| `compose` | `project, command: up\|down\|restart\|stop\|start\|pull` | `Result<(), UiError>`. Output goes into `Snapshot.compose_runs` |
| `subscribe_logs` | `id, channel: Channel<LogBatch>` | Subscription id. tail is the retained line count from the settings (max 10,000) |
| `subscribe_stats` | `id, channel: Channel<StatsSample>` | Subscription id |
| `exec_start` | `id, cols, rows, channel: Channel<ExecOutput>` | Session id |
| `exec_input` / `exec_resize` | `session, data: string` / `session, cols, rows` | `Result<(), UiError>` |
| `unsubscribe` | subscription id or session id | `()`. Stops the stream |
| `get_config` / `set_config` | `Config` | `Config` (context is changed only through `use_context`) |

Events: `state-changed` (payload is `Snapshot`, at most once per 50ms), `notice { kind: success|error, text }`

- Overlap with an in-flight operation: if an operation with the same key (container id, `compose:<project>`, pull reference, etc.) is already running, the command returns a `Conflict` error. Requests are never dropped silently; E2E found a bug where `down` was lost when operations were issued back to back before the display updated.
- compose arguments: `compose` receives only the project name. The working directory and compose file paths are looked up from the container labels held on the Rust side. The WebView cannot pass an arbitrary path and make `docker compose` run on it.
- Numeric types: sizes and timestamps are sent as `f64` (exact up to 2^53). specta emits `f64` as `number | null`, so fields that are always finite carry `#[specta(type = u32)]` to make the TS type `number`; this changes only the generated type annotation.
- Shared types: `ui/src/bindings.ts` is generated from the Rust types with `specta` + `tauri-specta`, so it cannot drift the way hand-written types would. Both are pinned to `=2.0.0-rc.26`; they are rc versions, so upgrade both together. Generate with `cargo test -p docker-stack export_bindings`. The output is committed, and CI regenerates it and checks that there is no diff.
- Out-of-order arrival: the `get_snapshot` response and a `state-changed` event can arrive in either order. `Snapshot.revision` increases on every state change, so the UI keeps only the one with the highest revision.
- Restricting exposed commands: only the commands listed in `AppManifest::commands` in `build.rs` exist, and of those, only the ones allowed with `allow-<command>` in `capabilities/default.json` can be called. Adding a command means updating both.
- Errors: `EngineError` is converted to `UiError { kind: "permission_denied"|"not_found"|"conflict"|"daemon"|"transport"|"local", message }`. The UI branches on `kind` only.
- Stream lifetime: the Rust side tracks streams in a `HashMap<id, JoinHandle>`. A subscription stops when one of these three happens:
  - `unsubscribe` is called
  - Sending to the Channel fails (the receiver is gone after a WebView reload or navigation)
  - The source stream ends (the container stopped, the connection dropped). For logs, a final batch is sent with the reason in `ended`

  The React side calls `unsubscribe` in the `useEffect` cleanup.

### Stream batching (controlling IPC load)

- logs: chunks received from engine are grouped every 50ms or every 64KB and sent as `LogBatch { chunks: {stream, text}[], ended }`. Consecutive chunks from the same stream are concatenated. The frontend splits lines and parses ANSI.
- exec output: grouped every 16ms and sent as base64. A UTF-8 character can be split mid-sequence, so the output is handled as bytes, not strings. Input is sent with `exec_input` on each xterm.js `onData`.
- pull progress: goes into `Snapshot.pulls` (a percentage computed from layer totals) and rides on the `state-changed` throttling.

## 5. Connection state (Rust side, ported from rev.1 `store.rs`)

```
Connecting ──ok──▶ Connected ──events lost──▶ Reconnecting ─┐
   │  └─err─▶ Unreachable ──backoff (1,2,4,…,30s)──▶ Connecting
   └─PermissionDenied ─▶ show guidance (manual retry only)
```

- The generation number is incremented on every connect and every context switch. Results and streams from older generations are discarded.
- In any state other than `Connected`, the UI dims the lists and disables action buttons. The lists are not cleared, so "0 containers" stays distinguishable from "disconnected".

## 6. Frontend

- Lists: `@tanstack/react-virtual` renders only the visible range. `lib/rows.ts` groups the container list (ported from `ui/rows.rs`, with the same test cases moved to vitest).
- Logs:
  - Each container has a ring buffer with a line limit (default 10,000 lines, configurable in the settings screen).
  - `lib/ansi.ts` parses ANSI (ported from `logbuf.rs`, tests included). Rendering is again limited to the visible range.
  - While following, the view auto-scrolls to the end; scrolling up stops following. Line search is supported.
  - Alternative, if the measurement in §1 had shown insufficient performance: move the ring buffer to the Rust side and fetch only the visible range with `get_log_range(sub, start, len)`.
- exec: uses `@xterm/xterm` and `@xterm/addon-fit`. Resize goes through `exec_resize`; copy and paste are Ctrl+Shift+C / V. xterm.js is about 330KB, so it is lazy-loaded when the Exec tab is opened. The shell is bash if available, otherwise sh.
- Image inspect: the `{}` button in the list shows the JSON at the bottom.
- stats: subscribed only while the Stats tab of the detail pane is shown. The chart draws the last 60 samples as SVG.
- Destructive operations: remove, prune and `compose down` show a confirmation dialog. prune also shows the number of affected items.
- Theme: follows `prefers-color-scheme`, with a manual light/dark choice. Colors are managed with CSS variables.
- Keyboard: Ctrl+F (search), Ctrl+R / F5 (reload), Ctrl+L (logs), Ctrl+E (exec), Delete (remove), Esc (close detail), Ctrl+1 to 4 / Ctrl+, (switch screens). Character-key shortcuts are disabled while an input field has focus.

## 7. Security (Tauri-specific)

- capability: only `core:default`, `core:event:default` and the app's own commands are allowed. The `shell`, `fs` and `http` plugins are not included. Only the Rust side launches compose.
- CSP: `default-src 'self'; style-src 'self' 'unsafe-inline'`. No external network access.
- All strings that come from containers, such as logs, inspect and compose output, are treated as untrusted data. They are never inserted as HTML (no `dangerouslySetInnerHTML`), and ANSI parsing results are also built as React elements.
- Confirmation for `remove_*` / `prune` happens on the frontend. The Rust side executes the commands it receives as is; the boundary is kept by limiting the kinds of commands.
- The CSP as implemented is `default-src 'self'; style-src 'self' 'unsafe-inline'; connect-src ipc: http://ipc.localhost` (`devCsp: null` during development).

## 8. Settings and persistence

Settings live in `$XDG_CONFIG_HOME/docker-stack/config.toml`, written to a temporary file and then renamed. It stores the context, retained log lines, theme, whether to show stopped containers, display language (system / en / ja), and whether to stay in the tray. `stacks.toml` in the same directory records the Compose projects per target (the working directory and Compose file paths read from container labels), so that a project can be brought `up` again from the GUI after `down`. Docker's own settings are read only.

## 9. Build and distribution

- Packages required to build (Ubuntu): `libwebkit2gtk-4.1-dev`, `libsoup-3.0-dev`, `libjavascriptcoregtk-4.1-dev`, `librsvg2-dev`, `build-essential`, plus Node.js 22 and npm.
- Installation: `scripts/install.sh` builds from source and installs. On apt systems it builds a `.deb`, on dnf/yum systems an `.rpm`, and installs it with sudo; with `--method user` it places the binary, `.desktop` file and icon under `~/.local` without sudo. On macOS it builds `Docker Stack.app` and copies it to `/Applications` (changeable with `--prefix`). `--deps` also installs build dependencies (packages on Linux, Xcode Command Line Tools on macOS), and `--uninstall` removes it.
- Development: `npm install` → `npm run tauri dev`. Debug build: `npm run tauri -- build --debug --no-bundle`.
- Tests: `cargo test --workspace` (Rust unit tests and type definition regeneration), `npm run typecheck`, `npm test` (vitest).
- Artifacts: `npm run tauri build` produces `.deb`, `.rpm` and AppImage under `target/release/bundle/` (the .deb has Depends: `docker-ce-cli | docker.io`, `libwebkit2gtk-4.1-0`, `libgtk-3-0`). Building the AppImage downloads linuxdeploy from GitHub. Flatpak stays deferred: reaching docker.sock from its sandbox requires punching a hole in it.

## 10. Implementation order

| M | Content | Done when |
|---|---|---|
| M0 | Tauri skeleton, `get_snapshot`, connection state display | On launch, the Engine version is shown. A user outside the docker group sees the permission error guidance |
| M1 | Container list and actions, logs (batched sending + virtual scrolling), **performance measurement** | With a container printing 10,000 lines per second, the WebView never freezes for 1 second or more and following does not lag (record the measurements) |
| M2 | Updates from events, reconcile, reconnect, notifications | `docker run` from the external CLI appears in the list within 1 second. Restarting dockerd recovers automatically |
| M3 | Images (pull progress), volumes, networks, prune | |
| M4 | exec (xterm.js) | `sh` runs and can be driven with resize, Ctrl+C and cursor keys |
| M5 | Compose grouping and actions, stats, inspect | |
| M6 | Settings screen, context switching, theme, keyboard control, building artifacts | The `.deb` installs and launches from the desktop menu |

Verification at each stage:
- `cargo test --workspace`: 8 engine unit tests, 15 src-tauri unit tests (including type definition regeneration). With `DOCKER_STACK_LIVE=1`, the 4 engine live tests also run against a real dockerd.
- `npm run typecheck`, `npm test` (18 vitest tests: rows, ansi, format, conn)
- `cargo clippy --workspace --all-targets` (0 warnings), `cargo fmt --all --check`
- E2E: `npm run tauri -- build --debug --no-bundle && node scripts/e2e.mjs`
  - Drives the real app through the WebKitGTK remote inspector. `tauri-driver` is not used (`WebKitWebDriver` is not available on this development machine).
  - The settings file points to a temporary directory. A socket proxy sits between the app and dockerd so the connection can be cut and reconnection tested. Only resources named `ds-e2e*` are created and removed, and prune is never run.
  - Result on 2026-11-10: 20/20 steps passed (Docker Engine 29.9.3).

| M | Confirmed by E2E |
|---|---|
| M0 | Engine version display. The permission error and unreachable displays were confirmed in M0 with `ui-eval.mjs` |
| M1 | Following logs with ANSI colors. Stop, start, kill, pause, unpause and remove (through the confirmation dialog) from the UI. Stopping ends the logs, and restarting resumes following. **At about 10,000 lines per second, the longest main-thread stall was 9-12ms, and the buffer caps at the 10,000-line limit** |
| M2 | An external `docker run` appears in the list in about 0.2-0.3 seconds. Cutting the connection shows "Connection lost" and the lists stay, dimmed. Restoring the connection recovers automatically |
| M3 | pull (success notification, reflected in the list), creating and removing volumes and networks, the remove button disabled for built-in networks (bridge), invalid names rejected with an error notification |
| M4 | A shell starts in xterm.js, and the result `42` of `echo $((6*7))` appears on screen (the unverified item carried over from rev.1 is resolved) |
| M5 | Receiving stats, inspecting containers and images, compose project grouping and stop, start, restart, down |
| M6 | Shortcuts Esc, Ctrl+2, Ctrl+,. Theme switching takes effect and is saved to the settings file. Context switching and saving |

Not confirmed by E2E:
- Running prune (it deletes the user's resources)
- Terminal input with Ctrl+C and cursor keys
- Ctrl+Shift+C / V
- Installing the `.deb` and launching from the menu (requires sudo. Instead, the `.deb` was unpacked and its Depends, `.desktop` file and icon were checked, and the binary was confirmed to launch)
- IME on Wayland
- The permission error guidance (only checked manually in M0)

## 11. Risks

- WebKitGTK performance and compatibility: large DOM updates and canvas drawing are slower than on Chromium, and xterm.js's WebGL renderer is unavailable in some environments. Measure in M1, and keep xterm.js able to fall back to the DOM / canvas renderer.
- IPC load: JSON serialization and IPC become the bottleneck if streams are not batched. The batch intervals in §4 are measured and tuned in M1.
- Two tokio runtimes: `crates/engine` has its own tokio runtime, and Tauri has another. `EngineTask` can be awaited from any executor, so this works correctly, at the cost of extra threads. If needed, let engine accept an external `Handle` and merge them into one.
- Migration to Tauri 3: 3.x is currently alpha. Implement against the 2.x API only, and decide on migration once a stable 3.x is released.
- When this design is wrong: if the M1 measurement shows that log display or terminal rendering on WebKitGTK is not usable, and keeping the visible range on the Rust side does not solve it, the choice of UI layer (including going back to a native UI) has to be made again.

## 12. Open questions

- Frontend framework: the default is React + TypeScript + Vite. If Svelte or Solid is preferred, the directory layout in §3 and the library choices in §6 are swapped out. Whether to use a UI component library (shadcn/ui, etc.) is also open; the default is to build without one, based on CSS variables.
- How the Engine is provided: connecting to the host's dockerd (A) is the default. Is the VM-managing approach (B) needed?
