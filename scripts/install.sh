#!/bin/sh
# Install the `dock` binary from this checkout. Rustc/cargo only — no Homebrew, no companions.
set -eu
if ! command -v cargo >/dev/null 2>&1; then
  echo "dock: cargo is required (install via https://rustup.rs)" >&2
  exit 1
fi
cd "$(dirname "$0")/.."
exec cargo install --path . --locked --force
