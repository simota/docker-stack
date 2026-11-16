#!/usr/bin/env bash
# One command for a person, an agent and CI (.github/workflows/ci.yml runs `--full`).
# Default: fmt and clippy always, then the tests that reach what changed since the
# merge-base with main. `--full`: every test. Exit 1: the last line names the checks
# that failed, and each one's full output is in .verify/<name>.log.
set -uo pipefail
cd "$(dirname "$0")/.."
mkdir -p .verify
rm -f .verify/*.log
: >.verify/timings

failed=()
run() { # run <name> <command> [args...]
  local name=$1 start=$SECONDS
  shift
  local log=".verify/$name.log"
  printf '== %s\n' "$name"
  if "$@" >"$log" 2>&1; then
    printf '   ok\n'
  else
    printf '   FAILED: see %s\n' "$log"
    tail -n 40 "$log"
    failed+=("$name")
  fi
  printf '%s\t%ss\n' "$name" "$((SECONDS - start))" >>.verify/timings
}

# The engine crate is shared: the app depends on it, so a change there runs every test.
widens='^(scripts/verify\.sh|\.github/|Cargo\.toml|Cargo\.lock|crates/engine/)'
scope=none
if [ "${1:-}" = "--full" ]; then
  scope=workspace
else
  while IFS= read -r path; do
    if [ ! -e "$path" ] || [[ $path =~ $widens ]]; then
      echo "verify: $path widens the change to every test"
      scope=workspace
      break
    fi
    [[ $path =~ ^app/ ]] && scope=app
  done < <({ git diff --name-only "${VERIFY_BASE:-$(git merge-base HEAD main)}" --; git ls-files --others --exclude-standard; } | sort -u)
fi

run fmt    cargo fmt --all -- --check
run clippy cargo clippy --workspace --all-targets -- -D warnings
case $scope in
  workspace) run test cargo test --workspace ;;
  app)       run test cargo test -p docker-stack ;;
  none)      echo "== test: skipped, no change under app/ or crates/" ;;
esac

echo "not covered: crates/engine/tests/live.rs against a real dockerd (DOCKER_STACK_LIVE=1, it creates and removes containers), release builds and scripts/install.sh, the window on screen, secret scanning"
if [ "${#failed[@]}" -gt 0 ]; then
  printf 'verify: FAILED: %s\n' "${failed[*]}"
  exit 1
fi
echo "verify: ok"
