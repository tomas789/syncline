#!/usr/bin/env bash
# Containerised version of the wdio e2e flow used in the README.
# Pass any wdio CLI args after `--`, e.g.:
#
#   docker compose run --rm e2e bash e2e/docker/run-tests.sh \
#     --spec test/specs/plugin-cross-vault-install.e2e.ts
#
# Steps:
#   1. cargo build --release --bin syncline   (Rust binary the spec spawns)
#   2. obsidian-plugin: npm install, build (wasm-pack + rollup)
#   3. e2e: npm install, run wdio under Xvfb
#
# Re-running is fast: the host-mounted target/, cargo cache, and the
# two node_modules volumes mean only changed files are recompiled.

set -euo pipefail

cd /workspace

echo "=== [1/3] cargo build --release --bin syncline ==="
cargo build --release --bin syncline

echo "=== [2/3] obsidian-plugin build (wasm-pack + rollup) ==="
pushd obsidian-plugin >/dev/null
# Prefer ci when a lockfile is present so we don't rewrite the host's
# package-lock.json on Linux (would diff vs. macOS host install).
if [[ -f package-lock.json ]]; then
    npm ci --no-audit --no-fund
else
    npm install --no-audit --no-fund
fi
npm run build
popd >/dev/null

echo "=== [3/3] wdio under Xvfb ==="
pushd e2e >/dev/null
if [[ -f package-lock.json ]]; then
    npm ci --no-audit --no-fund
else
    npm install --no-audit --no-fund
fi

# wdio-obsidian-service appends `--no-sandbox` on Linux automatically,
# so we don't need to set chromiumFlags here. We just need a $DISPLAY.
# `xvfb-run -a` picks an unused server number; the screen size is
# generous enough that Obsidian's onboarding modals don't clip.
exec xvfb-run -a --server-args='-screen 0 1280x900x24' \
    npx wdio run wdio.conf.ts "$@"
