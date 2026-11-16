# AGENTS.md

Rules for agents working in this repository. For usage and behaviour see [README.md](README.md); for where each feature's code and tests live see [docs/feature-map.md](docs/feature-map.md).

## Layout

- Two crates, with a one-way dependency: `app` (the `docker-stack` binary) → `crates/engine`. The UI toolkit is `noa-gpui-platform` from crates.io (declared in the root `Cargo.toml`)
- Talking to Docker stays inside `crates/engine`. bollard is a dependency of `engine` only, and bollard types never leave `engine` (see the top of `crates/engine/src/lib.rs`). Conversions from bollard models live in `crates/engine/src/ops.rs`
- The UI can run only the fixed set of functions in `app/src/commands.rs`. Do not add a path that passes arbitrary Docker API calls through (the socket is root-equivalent)
- The backend owns state in `app/src/state.rs`; the UI (`app/src/ui/`) draws snapshots of it. Types handed to the UI live in `app/src/dto.rs`
- Every string shown on screen goes into `app/src/ui/i18n.rs` in both English and Japanese
- Docker's own configuration (under `~/.docker`) is read, never written
- Change only what the task needs. Unrelated fixes go in a separate commit

## Coding

- New behaviour comes with tests, in the `#[cfg(test)] mod tests` at the end of each file
- Do not delete existing tests or mark them `#[ignore]` without a person's agreement
- Fix clippy findings and warnings. Do not silence them with `#[allow(...)]`
- Commit messages follow Conventional Commits (`feat: ...`, `fix: ...`, `refactor: ...`)

## Verification

- After a change, run `./scripts/verify.sh`. fmt and clippy run every time; tests run only for what the change reaches (everything when `crates/engine/`, `Cargo.toml` or `Cargo.lock` is touched). Before a pull request, run `./scripts/verify.sh --full` (the same as CI)
- A failed check is reported by name. Its full output is in `.verify/<check>.log`, and durations are in `.verify/timings`
- When the sandbox cannot bind a Unix socket, `target::tests::preflight_connects_async_and_times_out_or_errors_without_blocking` fails with `PermissionDenied`. In that case ask the person to run `! ./scripts/verify.sh --full` and include the result in your report
- `crates/engine/tests/live.rs` runs only with `DOCKER_STACK_LIVE=1`, and creates and removes containers, volumes and networks on the real daemon. Do not run it without a person's consent
- Check screen changes in the window (`cargo run -p docker-stack`). If you cannot run it, say in your report that it was not checked
- docs/feature-map.md gives, in its right-hand column, the command that runs only one feature's tests

## Safety

- Never print, commit or write secrets or credentials to files
- Ask a person before: adding or updating dependencies (`Cargo.toml` and `Cargo.lock`), changing `scripts/verify.sh` or `.github/`, or changing `scripts/install.sh` (it touches `sudo` and `/Applications`)

## Pull requests

- Fill in every section of `.github/pull_request_template.md`. Under "Verification", give the commands you ran and their results
- Write down what you did not check and what could go wrong
- Do not merge a pull request whose required checks fail

## What only review checks

Cargo enforces, from the declared dependencies, the `app` → `engine` direction and that `app` cannot use bollard. Neither `scripts/verify.sh` nor CI checks these rules from the sections above; review does:

- Layout: bollard types out of `engine`'s public API; no operations outside `commands.rs`; i18n strings in both English and Japanese
- Coding: tests for new behaviour; no deleted or `#[ignore]`d tests; no `#[allow(...)]`; the commit message format
- Safety: asking a person before changing dependencies, the verification gate or the install script (there is no CODEOWNERS)
- Pull requests: no merge until the required checks pass (`main` has no branch protection)
