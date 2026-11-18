#!/usr/bin/env bash
# Generate and sign the release manifest after all platform artifacts are available.
set -euo pipefail
cd "$(dirname "$0")/.."
exec python3 scripts/release.py "$@"
