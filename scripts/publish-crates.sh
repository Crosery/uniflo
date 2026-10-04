#!/usr/bin/env bash
# Publish Uniflo crates to crates.io in topological order.
# Usage:
#   scripts/publish-crates.sh --dry-run
#   scripts/publish-crates.sh
set -euo pipefail
cd "$(dirname "$0")/.."

DRY_RUN=""
if [[ "${1:-}" == "--dry-run" ]]; then
  DRY_RUN="--dry-run"
  printf "==> Running publish in dry-run mode\n"
fi

CRATES=(
  "uniflo-schema"
  "uniflo-core"
  "uniflo-search"
  "uniflo-adapters"
  "uniflo-gateway"
  "uniflo"
)

for crate in "${CRATES[@]}"; do
  printf "\n==> Publishing %s...\n" "$crate"
  cargo publish -p "$crate" ${DRY_RUN:+"$DRY_RUN"}
  if [[ -z "$DRY_RUN" ]]; then
    printf "Waiting 15s for %s to index on crates.io before downstream...\n" "$crate"
    sleep 15
  fi
done

printf "\n==> All uniflo crates published successfully!\n"
