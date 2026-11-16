# Quality assessment (SQI): design, architecture, visual design and UI/UX

This sheet rates the repository's design, architecture, visual design and UI/UX before and after improvement. The items, weights, criteria and measurement methods were fixed in this document **before any code changed** (the first commit), and scores after improvement use the same criteria.

SQI is a score from our own rubric, not a physical quantity. It has not been validated with real users (operation by an AI is not treated as a user test).

The environment constraints in §6 and §7 date from when noa-gpui was a private sibling checkout (`../noa-gpui`). It is now the `noa-gpui-platform` crate on crates.io, so the `app` crate and `./scripts/verify.sh` build without it.

## 1. Scope and conditions

- Scope: `crates/engine` and `app/src` (the UI is a native noa-gpui window). Generated files (`target/`), the external library (`noa-gpui-platform` from crates.io) and `Cargo.lock` are out of scope
- Screen conditions: the same as `pages_render_to_png` in `app/src/ui/mod.rs`. 1240×800, scale 1.0, light theme, English, the `sample` data from that test, CPU rendering (no GPU)
- Representative tasks (UXQI): T1 start and stop a container, T2 view logs, T3 delete a resource (a Volume, etc.), T4 bring a Stack down, T5 pull an image, T6 change a setting
- Weights: SQI = 0.25×DQI + 0.25×AQI + 0.25×VQI + 0.25×UXQI (all four areas in scope)
- Area score = Σ(weight × rating ÷ 5). Ratings are integers from 0 to 5. Unverified items are not scored; the area is then marked "provisional" and shown with its evaluation coverage (scored weight ÷ total weight). A provisional score is normalized by the scored weight
- Evidence levels: E0 unverified / E1 observation of code or screens / E2 reproducible static analysis, automated tests, or a fixed check procedure / E3 measurement, real usage conditions, or several independent sources
- Common scale: 0 fatal / 1 several serious defects / 2 the basics exist but problems recur / 3 meets basic requirements / 4 consistently high quality / 5 edge cases considered and confirmed by evidence (E2 or above). A 5 requires E2 or above

## 2. Measurement methods (how to reproduce)

| ID | Metric | Procedure |
|---|---|---|
| M1 | Test count and failures | `./scripts/verify.sh --full`, the `test result` lines in `.verify/test.log` |
| M2 | Dependency direction and boundary violations | Cargo dependency declarations (`app`→`engine` only), `grep -nE 'pub (fn\|struct\|enum\|type\|use).*bollard' crates/engine/src/*.rs`, `grep -rn bollard app/src` |
| M3 | Places where the UI reads backend state without going through the snapshot and `commands.rs` | `grep -nE 'backend\.(state\|streams)\(\)' app/src/ui/*.rs` (excluding tests) |
| M4 | String contracts duplicated across crates or layers | `grep -rn '"rootless (local)"' --include=*.rs app crates` |
| M5 | Contracts expressed as strings or always-identical values when a type could express them | Code reading (list in §4) |
| M6 | Unnamed controls (accessibility) | Build every page with the `sample` data and count the nodes in `UiTree::accessibility_update` of type Button / CheckBox / TextInput / SearchInput / Tab / ComboBox / Switch / Link / RadioButton / MenuItem / SpinButton / Slider that have neither label nor labelled_by (English and Japanese) |
| M7 | Contrast ratio | WCAG 2.x formula on the theme colors (`palette` in `app/src/ui/mod.rs`). Text 4.5:1; control borders (`outline`) 3:1 per 1.4.11 |
| M8 | Keyboard consistency | Of the 8 cells (4 list pages × {Esc (clear selection), Delete (delete selection)}), how many work |
| M9 | Ripple of change scenarios | S1 change the synthetic context name `rootless (local)` / S2 add keyboard handling to a resource page / S3 add an item to the tray menu / S4 add a UI string. Count the layers and crates touched in each scenario |
| M10 | Clipped or broken layout | In screenshots under the §1 conditions, count cells whose text is cut off with … and overlaps |

## 3. Items and criteria

### DQI (design quality)

| Item | Weight | Criteria (excerpt) |
|---|---:|---|
| D1 Separation of concerns | 20 | 5: M3 is 0 and no display formatting lives in the state layer (E2). 4: 1–2 minor mixes. 3: 3–5, or 1 serious mix. 2 or below: layers mixed widely |
| D2 Cohesion | 15 | 5: each module has a single purpose and a minimal public API. 4: large modules exist but are divided internally by purpose with little duplication. 3: several pages and shared components live in one file, and similar logic repeats across pages. 2: unrelated responsibilities mixed |
| D3 Interfaces and contracts | 15 | Count of M5. 5: 0. 4: 1–2. 3: 3–5. 2: 6 or more |
| D4 Ease of change and extension | 15 | Number of M9 scenarios that cross layers or crates needlessly. 5: 0 (E2). 4: 1. 3: 2. 2: 3 or more |
| D5 Readability | 10 | 5: consistent naming and documentation, no long functions. 4: consistent documentation, a few long functions (over 100 lines). 3: uneven documentation |
| D6 Testability | 15 | 5: every branch that decides on errors or reconnection is tested, and tests run without external dependencies. 4: tests run without external dependencies, but one piece of decision logic is untested. 3: two or more |
| D7 Invariants and error handling | 10 | 5: every unwrap/expect and swallowed error in non-test code is justified, and boundary values are tested. 4: almost all justified. 3: unjustified swallowed errors exist |

### AQI (architecture quality)

| Item | Weight | Criteria (excerpt) |
|---|---:|---|
| A1 Module boundaries and dependency direction | 20 | 5: 0 cycles, 0 M2 violations, 0 implicit contracts between crates (M4). 4: 1–2 implicit contracts. 3: boundary violations exist |
| A2 Layers and data flow | 15 | 5: one direction, backend→patch→snapshot→UI, and M3 is 0. 4: 1–2 exceptions. 3: 3 or more |
| A3 Locality of change | 15 | 5: every M9 scenario stays in one layer, confirmed at E2. 4: mostly local. 3: many span several layers |
| A4 Contracts and data consistency | 15 | 5: consistency mechanisms such as discarding stale updates and generation tracking are confirmed by tests. 4: mechanisms exist but tests cover only part |
| A5 Fault tolerance and isolation | 10 | 5: reconnection, timeouts and cancellation are tested and confirmed against a real daemon (E3). 4: tested but not confirmed against a real daemon |
| A6 Security and privacy boundaries | 10 | 5: the fixed set of operations, secret masking and input validation are tested exhaustively with no exceptions. 4: the main boundaries are tested, with minor exceptions |
| A7 Observability and operability | 10 | 5: structured logs, error display and verification procedures are in place and confirmed in real operation. 4: in place but not confirmed in real operation |
| A8 Scalability | 5 | 5: measured with large data. 4: flow control and throttling are tested, not measured |

### VQI (visual design quality)

| Item | Weight | Criteria (excerpt) |
|---|---:|---|
| V1 Visual hierarchy | 20 | 5: the main actions are obvious at a glance and emphasis is neither too much nor too little (E3). 4: clear hierarchy of headings, status badges and groups. 3: a hierarchy exists but the main actions are buried |
| V2 Consistency | 20 | 5: no values outside the tokens; the same meaning has the same look. 4: unified by tokens, a few exceptions. 3: inconsistencies recur |
| V3 Typography and legibility | 15 | 5: the text hierarchy is set by tokens, no clipping. 4: consistent hierarchy, a little M10 clipping in some states. 3: hard-to-read spots recur |
| V4 Layout and responsiveness | 15 | 5: M10 is 0 under the §1 conditions and with the detail panel open. 4: only unimportant columns clip. 3: meaningful information (counts, etc.) clips |
| V5 Interaction states | 10 | 5: Hover/Focus/Disabled/Selected/Loading/Error/Empty are all distinguishable, E2. 4: the main states are distinguishable |
| V6 Color, contrast and meaning | 10 | 5: the default palette meets M7's 4.5:1 for text and 3:1 for borders in both themes. 4: text passes, and 3:1 borders can be chosen in settings. 3: text passes but there is no way to get 3:1 borders. 2: text fails |
| V7 Spacing and information density | 10 | 5: all spacing comes from tokens and the density suits the work (E3). 4: consistent through tokens. 3: uneven |

### UXQI (UI/UX quality)

| Item | Weight | Criteria (excerpt) |
|---|---:|---|
| U1 Ease of task completion | 25 | 5: success rate measured with real users (E3). 4: T1–T6 each take 1–2 actions plus a confirmation (checked in code and on screen). 3: some task needs 3 or more actions |
| U2 Discoverability and cognitive load | 15 | 5: confirmed with real users. 4: icon-only buttons have tooltips, and shortcuts are shown. 3: some actions have no explanation |
| U3 Predictability and consistency | 15 | 5: confirmed with real users. 4: destructive actions always confirm, the same meaning uses the same icon, M8 is 8/8. 3: M8 below 8 |
| U4 Feedback | 10 | 5: progress and result of every action are shown, E2. 4: main actions show in-progress and result |
| U5 Error prevention and recovery | 10 | 5: input validation, confirmation and undo on every action, E2. 4: confirmation of destructive actions and input validation |
| U6 Accessibility | 15 | 5: M6 is 0, every action works by keyboard, checked manually with a screen reader (E3). 4: M6 is 0, 3:1 borders can be chosen (E2). 3: M6 is at most 10% of controls, or there is no way to get 3:1 borders. 2: M6 over 10% |
| U7 Perceived performance | 10 | Measure time from action to display on real hardware. This environment is headless CPU rendering, so unverified (E0) |

## 4. Before improvement (baseline)

State at commit `f21bb57`.

### Measurements

| ID | Result |
|---|---|
| M1 | fmt ok, clippy ok, 102 tests passed, 0 failed (app 67, engine 30; the 5 live tests do nothing without `DOCKER_STACK_LIVE`) |
| M2 | 0 cycles, 0 public bollard types, 0 bollard references from `app` |
| M3 | 1 (`status_menu` in `ui/mod.rs` locks and reads `backend.state().tray_summary()` from the UI thread. The `running/total` formatting is in the state layer, `state.rs`) |
| M4 | 1 kind, 7 places, 3 files, 2 crates (`"rootless (local)"`: 3 in `engine/target.rs`, 1 in `engine/compose.rs`, 3 in `app/ui/mod.rs`. i18n display strings excluded) |
| M5 | 4: (a) the M4 string, (b) `PageView::handle` returns `Option<Page>` but always `None`, (c) `Tr::ago(n, unit: &str)` takes the unit as a string and silently treats unknown units as "minutes", (d) table row button action names are strings (the spec of noa-gpui's `CellButton`) |
| M6 | 6 unnamed out of 73 controls (English, summed over pages) (8.2%). The same 6 in Japanese. All are text inputs (container search, log search, image to pull, new volume name, new network name, log retention lines). Screen readers read the placeholder, and log retention lines is read only as "10000" |
| M7 | All 36 text pairs at 4.5:1 or above (minimum 4.57, light accent on surface_alt). All 6 `outline` pairs (borders of inputs, buttons, checkboxes) below 3:1 (1.59–1.96). noa-gpui's `Theme::with_increased_contrast` can reach 3:1, but the app has no way to use it |
| M8 | 2/8 (Esc and Delete only on Containers. Nothing happens on Images, Volumes or Networks) |
| M9 | 2 scenarios cross layers or crates needlessly: S1 (2 crates, 3 files, 7 places), S3 (both the state layer and the UI layer). S2 and S4 stay in 1 layer |
| M10 | 0 on list-only screens. With the detail panel open, the Stack row counts (`2/3 running`, `down · 0/2 services`) clip: 2, plus clipping in the status and image columns |

### Scores

| Item | Weight | Rating | Weighted | Evidence | Level |
|---|---:|---:|---:|---|---|
| D1 | 20 | 4 | 16 | M3 = 1 | E2 |
| D2 | 15 | 3 | 9 | `ui/pages.rs` has 3,498 lines with 5 pages, the table components and the detail panel. Volumes and Networks repeat similar create/delete logic | E1 |
| D3 | 15 | 3 | 9 | M5 = 4 | E1 |
| D4 | 15 | 3 | 9 | M9 = 2 | E2 |
| D5 | 10 | 4 | 8 | Doc comments are consistent. Functions over 100 lines such as `container_grid_row` | E1 |
| D6 | 15 | 4 | 12 | The Driver trait, a fake client and headless UI tests run without external dependencies. The connection-lost check (`EngineError::classify`) has no tests | E2 |
| D7 | 10 | 4 | 8 | 13 non-test unwrap/expect (app 10, engine 3), justified (locks, runtime startup, etc.). `let _ =` on process exit and similar | E1 |
| DQI | 100 | | 71 | | Coverage 100% |
| A1 | 20 | 4 | 16 | M2 = 0, M4 = 1 | E2 |
| A2 | 15 | 4 | 12 | M3 = 1 | E2 |
| A3 | 15 | 4 | 12 | Most changes stay within a page or a layer | E1 |
| A4 | 15 | 5 | 15 | Tests for discarding stale patches by revision, list seq, and connection generations (`older_concurrent_refresh_does_not_overwrite_newer_list`, etc.) | E2 |
| A5 | 10 | 4 | 8 | Backoff, preflight timeout and stream flow control are tested. Not confirmed against a real daemon | E2 |
| A6 | 10 | 4 | 8 | Operations are a fixed set in `commands.rs`; env var masking in Inspect and name validation are tested. Image Inspect is not masked | E2 |
| A7 | 10 | 4 | 8 | tracing logs, error toasts, `verify.sh` | E1 |
| A8 | 5 | 4 | 4 | Log flow control and batching are tested, not measured with large data | E2 |
| AQI | 100 | | 83 | | Coverage 100% |
| V1 | 20 | 4 | 16 | Page headings, status badges and Stack group rows give a clear hierarchy. Header buttons all have the same weight | E2 (screenshots) |
| V2 | 20 | 4 | 16 | Spacing, type and lines from `tokens.rs`. Only the "Log retention lines" setting has its input misaligned with the other rows | E2 |
| V3 | 15 | 4 | 12 | 11–17 px tokens; IDs and ports are monospace | E2 |
| V4 | 15 | 3 | 9 | M10: Stack counts clip with the detail panel open | E2 |
| V5 | 10 | 4 | 8 | Disabled, Selected, empty and loading states are shown | E2 |
| V6 | 10 | 3 | 6 | M7: no way to get 3:1 borders | E2 |
| V7 | 10 | 4 | 8 | Spacing consistent through tokens | E2 |
| VQI | 100 | | 75 | | Coverage 100% |
| U1 | 25 | 4 | 20 | T1 and T2 take one row button press; T3 and T4 one press plus confirmation; T5 input plus Return; T6 one action | E1 |
| U2 | 15 | 4 | 12 | Tooltips on icon buttons, shortcuts in the navigation | E1 |
| U3 | 15 | 3 | 9 | M8 = 2/8 | E2 |
| U4 | 10 | 4 | 8 | Row buttons disabled while busy, pull progress, compose output, toasts | E1 |
| U5 | 10 | 4 | 8 | Confirmation for destructive actions, built-in networks cannot be deleted, name and line-count validation | E2 |
| U6 | 15 | 3 | 9 | M6 = 6/73 (8.2%), no way to get M7's 3:1 borders | E2 |
| U7 | 10 | — | — | Unverified | E0 |
| UXQI (provisional) | 90/100 | | 66/90 = 73.3 | | Coverage 90% |
| SQI | | | (71 + 83 + 75 + 73.3) / 4 = 75.6 | | |

### On bias

The M9 scenarios and the M5 list were chosen to include problems found during the investigation. The rise in D3, D4 and A1 from the improvements comes from fixing those problems; the effect on other change scenarios has not been checked.

## 5. After improvement

Covers the 9 commits from `f520194` (this document) to `3345136`. §3 applies unchanged.

### Changes and the items they affect

| Commit | Change | Items |
|---|---|---|
| `6884cbf` | The synthetic context name lives in one place, `engine::ROOTLESS_CONTEXT` | D3, D4, A1 |
| `5112712` | 3 tests for `EngineError::classify` and `is_connection_lost` | D6 |
| `413471d` | Tray summary moved from the state layer to a pure function in `ui/model.rs` (computed from the snapshot). The UI no longer locks backend state | D1, D4, A2 |
| `626240d` | `PageView::handle` no longer returns a value; the unit of `Tr::ago` is the enum `TimeUnit` | D3 |
| `1522180` | Names for the 6 inputs. A test that detects unnamed controls on every page in both languages | U6 |
| `99c2e04` | Esc (clear selection) and Delete (delete with confirmation) on Images, Volumes and Networks too | U3 |
| `34bd97f` | "Increase contrast" setting (`increase_contrast`, off by default) | V6, U6 |
| `eea45a3` | Japanese for the CPU meter unit (cores) | (Missing i18n. Does not change any item's level) |
| `3345136` | Adds a screenshot of the settings screen with increased contrast | For verification |

### Measurements

| ID | Before | After |
|---|---|---|
| M1 | 102 passed, 0 failed | 111 passed, 0 failed (app 73, engine 33, live 5). fmt and clippy ok |
| M2 | 0 | 0 |
| M3 | 1 | 0 |
| M4 | 7 places, 2 crates | Defined in 1 place (`engine/src/target.rs`) |
| M5 | 4 | 1 ((d) row button action names. Remains because of noa-gpui's spec) |
| M6 | 6 / 73 (same in both languages) | 0 / 74 (includes the new checkbox. `every_control_has_an_accessible_name` checks both languages and every page) |
| M7 | No way to get 3:1 borders | With the setting on, background, surface and surface_alt in both themes are all 3:1 or above (`increase_contrast_brings_control_borders_to_three_to_one`). The default palette is unchanged |
| M8 | 2/8 | 8/8 (3 tests) |
| M9 | 2 | 0 (S1 renames in 1 place; S3 touches only the UI layer) |
| M10 | Stack counts clip in the detail panel | Unchanged |
| Screen diff | — | 8 of the 9 screenshots under the §1 conditions are pixel-identical. Only Settings changed; the diff is the new checkbox and the shifted rows below it (x 16–496, y 257–659) |

### Scores

| Item | Weight | Before | After | Weighted diff | Evidence |
|---|---:|---:|---:|---:|---|
| D1 Separation of concerns | 20 | 4 | 5 | +4 | M3 1→0, formatting moved to the UI layer (E2) |
| D2 Cohesion | 15 | 3 | 3 | 0 | The structure of `pages.rs` is unchanged |
| D3 Contracts | 15 | 3 | 4 | +3 | M5 4→1 (E1) |
| D4 Ease of change | 15 | 3 | 5 | +6 | M9 2→0 (E2). See the bias note in §4 |
| D5 Readability | 10 | 4 | 4 | 0 | |
| D6 Testability | 15 | 4 | 5 | +3 | All branches of the connection-lost check tested (E2) |
| D7 Error handling | 10 | 4 | 4 | 0 | |
| DQI | | 71 | 87 | +16 | Coverage 100% |
| A1 Boundaries and dependency direction | 20 | 4 | 5 | +4 | M2 0, M4 0 (E2) |
| A2 Data flow | 15 | 4 | 5 | +3 | M3 0 (E2) |
| A3 Locality | 15 | 4 | 4 | 0 | |
| A4 Consistency | 15 | 5 | 5 | 0 | |
| A5 Fault tolerance | 10 | 4 | 4 | 0 | Still not confirmed against a real daemon |
| A6 Security | 10 | 4 | 4 | 0 | No operations added to `commands.rs` |
| A7 Observability | 10 | 4 | 4 | 0 | |
| A8 Scalability | 5 | 4 | 4 | 0 | |
| AQI | | 83 | 90 | +7 | Coverage 100% |
| V1–V5, V7 | 85 | — | — | 0 | Screen diff 0 (except Settings) |
| V6 Color and contrast | 10 | 3 | 4 | +2 | 3:1 borders can be chosen in settings (E2). Not 5 because the default does not meet it |
| VQI | | 75 | 77 | +2 | Coverage 100% |
| U3 Predictability | 15 | 3 | 4 | +3 | M8 2/8→8/8 (E2) |
| U6 Accessibility | 15 | 3 | 4 | +3 | M6 0, 3:1 borders selectable (E2). Not 5 because no manual screen reader check was done |
| U1, U2, U4, U5 | 60 | — | — | 0 | |
| U7 Perceived performance | 10 | — | — | — | Unverified (E0) |
| UXQI (provisional) | 90/100 | 73.3 | 80.0 | +6.7 | Coverage 90% |
| SQI | | 75.6 | 83.5 | +7.9 | |

### Changes not adopted

- Splitting `ui/pages.rs` (3,500 lines) into one file per page: D2 might rise, but the change is large, more files is not counted as a quality gain (§3), and with no change in behavior it is not worth the review load
- Sharing the create/delete logic of Volumes and Networks: similar, but the confirmation text is built differently; sharing it would add parameters and hurt readability
- Clipped Stack row counts with the detail panel open (V4): determined by how noa-gpui draws table group rows. The app could only change column widths, which would clip other columns
- Making the default palette's borders 3:1: it would change the look of every screen and goes against the existing design intent of faint, platform-matching borders. Made selectable in settings instead
- Typing the action names of row buttons (icon-only buttons) (M5 (d)), and exposing table cell buttons in the accessibility tree: both need changes on the noa-gpui side

### Still unverified

- U7 (perceived performance): not measured in an environment with a display. The only change was moving the tray summary (counting a few dozen items) from a state lock to a snapshot scan; the complexity is the same
- Display in a real window (`cargo run -p docker-stack`), macOS and Windows, screen reader output (VoiceOver, Orca), live tests against a real daemon
- Task success rate and time with real users

## 6. Round 2 (`228db4f` → this branch)

§3 applies unchanged. Rather than take the final state of §5 as the baseline, `228db4f` was rescored.

### Environment constraints

These constraints set the strength of the evidence for this round.

- The UI toolkit `../noa-gpui` (private) was not available in this environment, so the `app` crate could not be built as is. `./scripts/verify.sh` also stopped at `cargo metadata`
- Tests and clippy (`-D warnings`) ran instead in a scratch workspace (outside the repository) that referenced the non-UI modules of `app/src` as is (`backend`, `commands`, `config`, `dto`, `files`, `logging`, `run_spec`, `stacks`, `state`, `streams`, `ui/i18n`, `ui/model`, `ui/tokens`) and replaced only `ui_platform::Waker` with an empty type. fmt was checked by running `rustfmt --edition 2024 --check` directly on every `.rs`
- `ui/mod.rs`, `ui/pages.rs`, `ui/widgets.rs`, `ui/icons.rs` and `ui/stack_map.rs` could be neither compiled nor tested, so this round **did not change these files at all** (`git diff 228db4f -- app/src/ui/mod.rs app/src/ui/pages.rs app/src/ui/widgets.rs app/src/ui/icons.rs app/src/ui/stack_map.rs` is empty). M6, M10 and screenshots could not be measured

### Rescoring the baseline (differences from the final §5 values)

| Item | Final in §5 | Baseline this round | Reason (newly found evidence; same criteria) |
|---|---:|---:|---|
| D1 | 5 | 4 | A 5 requires "no display formatting in the state layer". `state.rs`, `commands.rs`, `streams.rs` and `run_spec.rs` build English notice and error text (about 40 places), and the UI shows it as is (in English even on the Japanese UI). One systematic kind of mixing, so 4 |
| D3 | 4 | 4 | M5 is 2: (d) from §5, and (e) the Compose busy key `"compose:{project}"` is built as a string in 3 places (2 in `state.rs`, 1 in `ui/pages.rs`). Same level |
| D6 | 5 | 3 | 4 functions make decisions but have no tests: `run_op_with` (busy check, failure notice and connection-lost routing for every write operation), `start_daemon_with` (whether to reconnect right after startup; calls `engine::start_daemon` directly, so it cannot be swapped for a fake), ignoring stale generations in `connection_lost_with`, and ignoring stale generations in `schedule_retry_with` |
| A4 | 5 | 4 | Saves of `stacks.toml` are not ordered. Each container list fetch and each "Remove from list" writes the same `stacks.toml.tmp` on a separate thread and renames it, so concurrent saves can fail the rename, or an older ledger can be written later and bring back a removed Stack. An unreadable file starts an empty ledger, which the next save writes over (all remembered Stacks are lost) |

### Changes and the items they affect

| Commit | Change | Items |
|---|---|---|
| `b016680` | `stacks.toml` saves take a ticket (while holding the state lock) and writes are serialized. Saves with an older ticket are dropped | A4, D7 |
| `3018b62` | One list refetch after Prune (8 requests → 4). `run_op` already refetches the lists and DISK, so the `refresh_all` in `on_ok` was removed | A8 (level unchanged) |
| `b690493` | 5 tests for `run_op_with` (busy, a different key passes, on success each list is refetched once and DISK is read, failure notice and key release, connection lost reconnects instead of notifying, a result from a stale generation does not touch the new state) | D6 |
| `29982c3` | `start_daemon_with` goes through `Driver::start_daemon` (same shape as `run_compose`). 4 tests, plus a test for connection loss from a stale generation | D6 |
| `5473559` | An unreadable `config.toml` or `stacks.toml` is moved to `*.invalid` before starting with defaults (the next save no longer wipes manual edits). The atomic write duplicated in 2 files moved to `files.rs` | D7, U5 (level unchanged) |
| `3115d1d` | Test that `schedule_retry_with` reconnects only for the same generation | D6 |

### Measurements

| Metric | Before | After | Method |
|---|---|---|---|
| Non-UI app tests | 61 passed, 0 failed | 77 passed, 0 failed (5 runs in a row) | `cargo test -p docker-stack` in the scratch workspace |
| engine tests | 41 passed (the 6 live tests do nothing without `DOCKER_STACK_LIVE`) | Same (engine unchanged) | `cargo test -p engine` |
| clippy `-D warnings`, rustfmt | ok | ok | As above |
| Concurrent `stacks.toml` saves (16 threads, ledgers of different lengths) | With the fix removed, 3 of 3 runs failed (I/O error on rename), and a save with an older ticket overwrote newer content | Pass; the content of the newest ticket remains | `concurrent_saves_leave_a_whole_file_from_the_newest_ticket`, `a_save_older_than_the_last_one_written_is_dropped` (failure confirmed by temporarily removing the fix) |
| List requests per Prune | 8 | 4 | Code reading (`commands::prune`) and the test that `run_op_with` refetches each list once. Not counted against a real daemon |
| Untested functions that decide on errors or reconnection | 4 | 0 (the 4 above) | Search the test module of `state.rs` by function name |
| An unreadable settings file lost on the next save | Lost | Kept as `*.invalid` | `a_config_that_does_not_parse_is_kept_and_not_saved_over` and 2 others |
| Run time of the non-UI app tests | 0.19 s | 2.34 s | One test waits a real reconnection backoff (in seconds). tokio's `test-util` (a dependency change) could shorten it |
| M2, M3, M4 | 0, 0, 0 | 0, 0, 0 | The greps in §2 |

### Scores

| Item | Weight | Before | After | Weighted diff | Evidence |
|---|---:|---:|---:|---:|---|
| D1 Separation of concerns | 20 | 4 | 4 | 0 | The state layer that builds English text is unchanged |
| D2 Cohesion | 15 | 3 | 3 | 0 | `pages.rs` unchanged. `files.rs` removed duplication but the level is unchanged |
| D3 Contracts | 15 | 4 | 4 | 0 | M5 still 2 |
| D4 Ease of change | 15 | 5 | 5 | 0 | |
| D5 Readability | 10 | 4 | 4 | 0 | `start_usage_with` (about 170 lines) unchanged |
| D6 Testability | 15 | 3 | 4 | +3 | Tests for the 4 above (E2). Not 5 because not every error branch of `pull_with`, `compose_with`, etc. was examined |
| D7 Invariants and error handling | 10 | 4 | 4 | 0 | Handling of unreadable files improved, but a failed `stacks.toml` save is only logged and not shown on screen |
| DQI | | 77 | 80 | +3 | Coverage 100% |
| A1 | 20 | 5 | 5 | 0 | |
| A2 | 15 | 5 | 5 | 0 | |
| A3 | 15 | 4 | 4 | 0 | |
| A4 Contracts and data consistency | 15 | 4 | 5 | +3 | Ordering and exclusion of persistence, dropping stale tickets and preserving unreadable files confirmed by tests (E2; they fail with the fix removed) |
| A5 | 10 | 4 | 4 | 0 | Not confirmed against a real daemon |
| A6 | 10 | 4 | 4 | 0 | Image Inspect still not masked |
| A7 | 10 | 4 | 4 | 0 | |
| A8 | 5 | 4 | 4 | 0 | Prune requests halved, but no measurement with large data |
| AQI | | 87 | 90 | +3 | Coverage 100% |
| VQI | | 77 | 77 | 0 | §5 ratings kept (V4 3, others 4). No UI files changed this round and screenshots could not be taken, so not rechecked (E1) |
| UXQI (provisional) | 90/100 | 80.0 | 80.0 | 0 | U7 unverified. Preserving unreadable settings helps U5's "entered data is not lost", but the level does not change (5 requires undo on every action) |
| SQI | | 80.3 | 81.8 | +1.5 | (77 + 87 + 77 + 80.0) / 4 → (80 + 90 + 77 + 80.0) / 4 |

### Changes not adopted, and why

- Localizing the English text in the state layer (D1, U4; the biggest gain for Japanese users): the minimal fix is to add codes to `UiError` and `Notice` and translate them with `Tr`, but that requires changing the display side, `ui/mod.rs` and `ui/pages.rs`, which could not be compiled in this environment
- Building the Compose busy key in one function (D3): 1 of the 3 places is in `ui/pages.rs`, and fixing only 2 would split the contract halfway
- Masking image Inspect (A6): masking also needs a toggle to show values (so information is not hidden for good). That is a change to `ui/pages.rs`
- Rejecting an empty tag such as `nginx:` early in the Run form: images are usually picked from a list row rather than typed, and the daemon error makes it clear. Small benefit
- Splitting `start_usage_with` (about 170 lines) (D5): no change in behavior or level

### Still unverified

- Building the whole `app` crate, the UI tests (`pages.rs`, `ui/mod.rs`, `stack_map.rs`), `./scripts/verify.sh --full`, and display in a window. Only non-UI files changed, and no signatures of public functions used by the UI changed (`Driver` is implemented only inside `state.rs`), but this needs checking in an environment with noa-gpui
- Live tests against a real daemon, measuring Prune request counts, U7, checks with real users

## 7. Round 3 (`421452c` → this branch)

§3 applies unchanged. PR #7 (Duplicate, abnormal-exit notices) landed after the final state of §6, so `421452c` was rescored. The "before" part of this section was committed **before any code changed**.

### Environment constraints

Same as §6. Because `../noa-gpui` was not available, tests and clippy (`-D warnings`) ran in a scratch workspace (outside the repository) that referenced the non-UI modules of `app/src` plus `ui/i18n.rs`, `ui/model.rs` and `ui/tokens.rs` via `#[path]`, with only `ui_platform::Waker` replaced by an empty type. engine tests and clippy ran at their normal paths, and fmt ran `rustfmt --edition 2024 --check` directly on every `.rs`. `ui/mod.rs`, `ui/pages.rs`, `ui/widgets.rs`, `ui/icons.rs` and `ui/stack_map.rs` were again left unchanged. M6, M10 and screenshots could not be measured; VQI and U1–U6 therefore carry over the §6 ratings at E1 (code reading).

### Baseline measurements

| Metric | Value | Method |
|---|---|---|
| Non-UI app tests | 93 passed, 0 failed | `cargo test -p docker-stack` in the scratch workspace |
| engine tests | 49 passed, 0 failed (the 6 live tests do nothing without `DOCKER_STACK_LIVE`) | `cargo test -p engine` |
| clippy `-D warnings`, rustfmt | ok, ok | As above |
| M2, M3, M4 | 0, 0, 0 | The greps in §2 (the 4 M3 hits from line 3727 of `pages.rs` onward are inside the test module) |
| M5 | 2 ((d) and (e) from §6) | `grep -rn 'format!("compose:' app/src` finds 3 places |
| Functions over 100 lines (non-test) | 8 (2 in `state.rs`, 6 in the UI) | Counted with a simple brace-matching script |
| Untested branches that decide on errors or reconnection | 4 in `pull_with` (reject duplicate, failure notice, connection lost, refetch on success), 3 in `compose_with` (reject duplicate, ends in failure, ends without Exit), failure and connection loss in background reads (capacity, lists, usage, disk) | Search the test module of `state.rs` by function name and notice text |

### Newly found defects

Found in this round's investigation: a separate agent read the non-UI code without being given my assessment, and each of its reports was checked against the code one by one.

| # | Defect | Check |
|---|---|---|
| F1 | An error the daemon returns mid-pull (disk full, layer fetch failed, etc.) becomes `DockerStreamError` in bollard 0.21.1, and `EngineError::classify` maps that to `Transport` (connection lost). The whole app reconnects, other pulls, Compose runs and log and exec streams stop, and the pull's error text is never shown. If the text contains "permission denied", the docker group hint appears and no retry happens | bollard `image.rs` lines 136–148 and `errors.rs` line 79, `classify` in `error.rs`. The `error_detail` branch in `ops.rs` is never reached |
| F2 | The engine-side task of a write operation starts before `run_op`'s busy check (`spawn_op` runs as soon as it is created). Pressing restart twice gets the second press refused as "busy", but the request has already reached the daemon. Affects `container_action`, `remove_image`, `remove_volume`, `remove_network`, `create_volume`, `create_network` and `prune`. Only `run_container` is deferred | `spawn_op_with_timeout` in `ops.rs` and each function in `commands.rs` |
| F3 | A recorded `oom` event stays until the next `die`. If one process inside a container is OOM-killed while the container keeps running, and hours later it stops normally, the app reports "stopped: out of memory" even with exit code 0 | `observe` in `exits.rs` |
| F4 | If `ESC ]` (OSC) or `ESC [` has no terminator and 256 or more bytes follow, the rest of that chunk (including complete lines) is discarded | `LogBuffer::push` and `escape_len` in `ui/model.rs` |
| F5 | Duplicate silently drops the count from `on-failure:5`, so the copy restarts without limit. A volume whose destination contains spaces or commas (`data:/srv/my data`) is accepted by the form but fails to parse on Run. Neither appears in the list of "settings that cannot be carried over" | `RestartPolicy` in `template.rs`, `entries` and `form_from_spec` in `run_spec.rs` |
| F6 | The 120-second operation timeout (`spawn_op` and bollard's `RequestTimeoutError`) becomes `Transport`, so stopping a container that takes over 2 minutes to stop (Compose `stop_grace_period: 3m`) makes the whole app reconnect | `ops.rs` line 95, `classify` in `error.rs` |
| F7 | Signals that do not stop the container, such as `docker kill -s HUP` (config reload), are also recorded as "stopped", so a real abnormal exit within 5 minutes is not reported. A `kill` during an event stream outage is not seen, and a `die` 143 after reconnection may be reported as an abnormal exit | `exits.rs`. Signals are not in `EngineEvent`. The actual ordering was not checked against a real daemon (PLAUSIBLE) |

### Rescoring the baseline (differences from the final §6 values)

| Item | Final in §6 | Baseline this round | Reason (same criteria) |
|---|---:|---:|---|
| D6 | 4 | 4 | The untested branches in the table above. Same level as §6 |
| D7 | 4 | 3 | 3 means "unjustified swallowed errors exist". With F1, the pull error text is discarded and never reaches the user |
| A4 | 5 | 4 | 4 means "mechanisms exist but tests cover only part". The busy contract (operations with the same key do not run) is broken for 7 operations by F2, and tests only check it with futures that do nothing |
| A5 | 4 | 3 | The failure of one request spreads to a reconnection of the whole app (F1, F6). Faults are not isolated |

### Baseline scores

| Area | Ratings (weight) | Score |
|---|---|---:|
| DQI | D1 4(20), D2 3(15), D3 4(15), D4 5(15), D5 4(10), D6 4(15), D7 3(10) | 78 |
| AQI | A1 5(20), A2 5(15), A3 4(15), A4 4(15), A5 3(10), A6 4(10), A7 4(10), A8 4(5) | 85 |
| VQI | As in §6 (V4 3, V6 4, others 4). E1 | 77 |
| UXQI (provisional) | As in §6. U7 unverified. Coverage 90% | 80.0 |
| SQI | (78 + 85 + 77 + 80.0) / 4 | 80.0 |

### Changes in scope for this round (planned)

Fixes for F1–F5, and tests for the untested branches above. F6 is on hold because fixing it brings a different behavior change (the reconnecting indicator would no longer appear when the daemon hangs). F7 is on hold because it needs signals added to the engine events and cannot be checked against a real daemon.

### After improvement (from the commit after `a71edb0`)

§3 applies unchanged, and scoring used the baseline's evidence conditions.

#### Changes and the items they affect

| Commit | Change | Items |
|---|---|---|
| `27ac9df` | `DockerStreamError` (an error the daemon returns mid-operation) is classified as `Daemon` (F1). A failed pull becomes a notice for that pull, and the whole app does not reconnect | D7, A5 |
| `9936f2b` | `run_op` takes a "function that builds the request" and calls it only after taking the busy key (F2). The ordering is enforced by the signature, not by convention. The signatures of the public functions in `commands.rs` are unchanged | A4, D3 |
| `e09157b` | `oom` explains only a `die` within 10 seconds (F3) | U4 |
| `53f2893` | An unterminated escape is discarded only up to the end of its line (F4). The existing "discard after 256 bytes" rule is kept | U4 |
| `70ef9c9` | Duplicate lists the `on-failure:N` count, and volumes the form would split, under "settings not duplicated" (F5). README (Japanese and English) updated too. For the count, PR #11 merged into `main` already did the same with `SkippedSetting::Option` (`--restart on-failure:N`), so my `RestartRetries` was dropped when merging `main` | U5 |
| (merge of `main`) | The update (`update_container`) and network connect/disconnect added by PR #11 did not compile with the new `run_op` shape, which cannot build a request before the busy check, so they were changed to the same shape. Pressing Update twice could have run two replacements of the same container side by side | A4 |
| `bc1efa7`, `446ad32`, `a54f615` | 14 tests: 4 pull branches, 3 Compose branches, 4 kinds of background read × {connection lost, other failure}, connection loss on the first capacity read, how log streams end | D6 |

#### Measurements

| Metric | Before | After | Method |
|---|---|---|---|
| Non-UI app tests | 93 passed, 0 failed | 106 passed, 0 failed (3 runs in a row) | `cargo test -p docker-stack` in the scratch workspace |
| engine tests | 49 passed | 50 passed (the 6 live tests do nothing) | `cargo test -p engine` |
| clippy `-D warnings`, rustfmt | ok, ok | ok, ok | As above |
| Confirmed defects fixed | — | 5 / 7 (F1–F5. F6 and F7 on hold) | "Changes not adopted" below |
| Write operations that send the request to the daemon before the busy check | 7 | 0 | Reading `commands.rs`, and a test that the start function of a refused second request is not called (fails if `start()` is moved before the check) |
| Untested branches that decide on errors or reconnection (those listed in the baseline table) | 15 (pull 4, Compose 3, 4 kinds of read × 2) | 0 | All 11 mutants, each breaking one branch in `state.rs`, were caught by the tests |
| Tests that fail with the fix removed | — | Confirmed for F1, F2, F4 | Ran with the fix temporarily reverted |
| Run time of the non-UI app tests | 2.34 s | 2.47 s | The waits (tens of milliseconds) in the added tests |
| M2, M3, M4, M5 | 0, 0, 0, 2 | 0, 0, 0, 2 | The greps in §2 |

#### Scores

| Item | Weight | Before | After | Weighted diff | Evidence |
|---|---:|---:|---:|---:|---|
| D1 Separation of concerns | 20 | 4 | 4 | 0 | English text in the state layer unchanged |
| D2 Cohesion | 15 | 3 | 3 | 0 | `pages.rs` unchanged |
| D3 Contracts | 15 | 4 | 4 | 0 | M5 still 2. The busy ordering is now expressed in types, but that is outside M5 |
| D4 Ease of change | 15 | 5 | 5 | 0 | |
| D5 Readability | 10 | 4 | 4 | 0 | Still 8 functions over 100 lines |
| D6 Testability | 15 | 4 | 5 | +3 | Every branch listed in the baseline is tested, confirmed by mutation (E2) |
| D7 Invariants and error handling | 10 | 3 | 4 | +2 | Pull errors are no longer discarded. Not 5 because a failed `stacks.toml` save is still only logged |
| DQI | | 78 | 83 | +5 | Coverage 100% |
| A1–A3 | 50 | — | — | 0 | |
| A4 Contracts and data consistency | 15 | 4 | 5 | +3 | The busy contract holds for every operation, confirmed by tests (E2; they fail with the fix removed) |
| A5 Fault tolerance | 10 | 3 | 3 | 0 | F1 fixed, but F6 (reconnect on timeout) remains |
| A6–A8 | 25 | — | — | 0 | |
| AQI | | 85 | 88 | +3 | Coverage 100% |
| VQI | | 77 | 77 | 0 | No screen files changed (E1) |
| UXQI (provisional) | 90/100 | 80.0 | 80.0 | 0 | F3, F4 and F5 help U4 and U5 but do not change the level. U7 unverified |
| SQI | | 80.0 | 82.0 | +2.0 | (78 + 85 + 77 + 80.0) / 4 → (83 + 88 + 77 + 80.0) / 4 |

#### Changes not adopted, and why

- F6 (reconnect on the 120-second timeout): taking timeouts out of "connection lost" would remove needless reconnections for slow-stopping containers, but the "Reconnecting" indicator would no longer appear when the daemon hangs. Which to prefer is the maintainer's call. It also needs a new kind in `EngineError` (a change to engine's public API), and cannot be checked against a real daemon
- F7 (`kill` signals not observed): needs an engine change to add signals to `EngineEvent`, and the order in which Docker emits events cannot be checked against a real daemon
- Localizing the English text in the state layer (D1), a function for the Compose busy key (D3), masking image Inspect (A6): same reasons as §6 (they need changes to `ui/mod.rs` and `ui/pages.rs`, which could not be compiled in this environment)
- Splitting `start_usage_with` (170 lines) (D5): no change in behavior, and the long UI functions remain, so the level would not change

#### Still unverified

- Building the whole `app` crate and the UI tests, `./scripts/verify.sh --full`, and display in a window. The signatures of the public functions in `commands.rs` that the UI calls are unchanged. The variants added to `dto::SkippedSetting` are not matched in the UI files (`grep -rn SkippedSetting app/src/ui/pages.rs app/src/ui/mod.rs` is empty)
- Daemon errors in the middle of a real pull (F1), the order of OOM events (F3), live tests. The evidence for F1 is reading the bollard 0.21.1 source and unit tests
- U7, checks with real users
