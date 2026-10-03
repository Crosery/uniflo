#!/usr/bin/env bash
# Unified acceptance gate: format, lint, every adapter feature alone, tests.
#   scripts/verify.sh          # what CI runs
#   scripts/verify.sh --e2e    # + headless-Chrome check of the web demo (needs bun + Chrome)
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n==> %s\n' "$*"; }

step "rustfmt"
cargo fmt --all -- --check

step "clippy (deny warnings)"
cargo clippy --workspace --all-targets -- -D warnings

step "each adapter feature builds alone"
features=$(sed -n '/^default = \[/,/\]/p' crates/uniflo-adapters/Cargo.toml | tr -d '[]" \t\n' | sed 's/^default=//' | tr ',' ' ')
RUSTFLAGS="-D warnings" cargo check -q -p uniflo-adapters --all-targets --no-default-features
for f in $features; do
  RUSTFLAGS="-D warnings" cargo check -q -p uniflo-adapters --all-targets --no-default-features --features "$f"
  printf '  %s ok\n' "$f"
done

step "branch invariants"
node scripts/check-branch-invariants.mjs

step "tests"
cargo test --workspace -q

if [[ "${1:-}" == "--e2e" ]]; then
  step "web demo e2e (headless Chrome)"
  cargo build --release -q
  bun scripts/demo-e2e.mjs
fi

printf '\nverify: all checks passed\n'
