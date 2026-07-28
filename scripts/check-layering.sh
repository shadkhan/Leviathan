#!/usr/bin/env bash
#
# Enforce the layering contract that makes leviathan-core reusable.
#
# The claim "the Rust core is independently publishable and embeddable" is worth
# nothing if it rests on nobody having added a convenient dependency yet. This
# script is what turns it into a property of the build:
#
#   leviathan-core may not depend on wasm-bindgen, js-sys, web-sys, or an async
#   runtime, and may not perform I/O.
#
# It is sans-IO by construction (DEEP_REASONING.md C2), which is why the same
# crate runs in a Web Worker, a native CLI, and the future MCP server unchanged.
#
# Runs in CI on every push, and locally via `pnpm check`.

set -euo pipefail

cd "$(dirname "$0")/.."

CRATE=leviathan-core
SRC=crates/leviathan-core/src

fail=0
note() { printf '  %-6s %s\n' "$1" "$2"; }
bad() { note FAIL "$1"; fail=1; }

echo
echo "layering: $CRATE"
echo

# --- 1. Dependency graph ------------------------------------------------------
#
# `--edges normal,build` deliberately excludes dev-dependencies: a test-only
# dependency does not ship to consumers and does not compromise portability.

FORBIDDEN_DEPS=(wasm-bindgen js-sys web-sys tokio async-std smol mio)

tree=$(cargo tree --package "$CRATE" --edges normal,build --prefix none --no-dedupe 2>/dev/null)

for dep in "${FORBIDDEN_DEPS[@]}"; do
  if grep -qE "^${dep} v" <<<"$tree"; then
    bad "$CRATE depends on $dep"
  fi
done

# The strongest form of the guarantee: no transitive dependencies at all. Not a
# hard requirement — if M1 needs one, this relaxes to the list check above — but
# while it holds it is worth saying out loud.
deps=$(grep -cE '^[a-z0-9_-]+ v' <<<"$tree" || true)
if [ "$deps" -le 1 ]; then
  note ok "no external dependencies"
else
  note ok "no forbidden dependencies ($((deps - 1)) external)"
fi

# --- 2. I/O in the source -----------------------------------------------------
#
# A dependency check cannot catch `std::fs`, which needs no dependency at all.

FORBIDDEN_USES=(
  'std::fs'
  'std::net'
  'std::io'
  'std::process'
  'std::thread'
  'std::time::SystemTime'
)

for pattern in "${FORBIDDEN_USES[@]}"; do
  # Exclude comment lines: this file's own contract is documented in prose
  # inside the crate, and prose is not a violation.
  if grep -rn --include='*.rs' -- "$pattern" "$SRC" | grep -vE '^\s*[^:]+:[0-9]+:\s*(//|\*)' | grep -q .; then
    bad "$SRC uses $pattern"
    grep -rn --include='*.rs' -- "$pattern" "$SRC" | grep -vE '^\s*[^:]+:[0-9]+:\s*(//|\*)' | sed 's/^/         /'
  fi
done
[ "$fail" -eq 0 ] && note ok "no I/O in $SRC"

# --- 3. The crate builds without std's host assumptions -----------------------
#
# If it compiles for wasm32 with no target-specific cfg, it is portable in the
# way that matters.

if cargo check --package "$CRATE" --target wasm32-unknown-unknown --quiet 2>/dev/null; then
  note ok "compiles for wasm32-unknown-unknown"
else
  bad "$CRATE does not compile for wasm32-unknown-unknown"
fi

echo
if [ "$fail" -ne 0 ]; then
  echo "layering contract violated — see DEEP_REASONING.md C2 before relaxing it."
  echo
  exit 1
fi
echo "layering contract holds."
echo
