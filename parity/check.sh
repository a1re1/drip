#!/usr/bin/env bash
# Differential parity harness entry point (see drip/parity/README.md).
#
#   drip/parity/check.sh                # compare Rust `drip` against the lci oracle
#   drip/parity/check.sh --self-check   # run lci on both sides (harness sanity check)
#   drip/parity/check.sh --only <name>  # run a single scenario
#   drip/parity/check.sh --keep         # keep temp roots for debugging (with --self-check)
#
# Step 1 runs the unit tests for the normalization layer.
# Step 2 runs the actual differential scenarios.
set -euo pipefail
cd "$(dirname "$0")/../.."

echo "==> vitest: drip/parity unit tests"
bun run vitest run drip/parity

echo "==> parity scenarios"
exec bun run drip/parity/run.ts "$@"
