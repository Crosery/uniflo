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
  "uniflo-client"
  "uniflo-core"
  "uniflo-adapters"
  "uniflo-search"
  "uniflo-gateway"
  "uniflo"
)

for crate in "${CRATES[@]}"; do
  printf "\n==> Publishing %s...\n" "$crate"
  attempts=0
  max_attempts=8
  while true; do
    attempts=$((attempts + 1))
    if out=$(cargo publish -p "$crate" ${DRY_RUN:+"$DRY_RUN"} 2>&1); then
      printf "%s\n" "$out"
      if [[ -z "$DRY_RUN" ]]; then
        printf "Waiting 20s for %s to index on crates.io before downstream...\n" "$crate"
        sleep 20
      fi
      break
    else
      if [[ "$out" == *"already uploaded"* ]] || [[ "$out" == *"already exists"* ]]; then
        printf "%s is already published at current version, skipping.\n" "$crate"
        break
      elif [[ "$out" == *"failed to select a version"* ]] && [[ $attempts -lt $max_attempts ]]; then
        printf "Dependency not indexed on crates.io yet (attempt %d/%d), waiting 15s before retry...\n" "$attempts" "$max_attempts"
        sleep 15
      else
        printf "%s\n" "$out" >&2
        exit 1
      fi
    fi
  done
done

printf "\n==> All uniflo crates published successfully!\n"
