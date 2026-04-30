# Headless e2e in a container

This directory packages the wdio + Obsidian e2e suite into a
container so it can run on a CI server (or any machine without a
display) without installing Rust, Node, Obsidian, or an X server on
the host.

The image and compose file work with both **Podman** (the local
default - see commands below) and **Docker**. The Dockerfile is
plain `node:20-bookworm` plus apt deps and a Rust toolchain; the
compose file uses standard Compose v3 syntax.

Verified working: both `pre-existing-vault.e2e.ts` and
`plugin-cross-vault-install.e2e.ts` run inside the container under
Xvfb and pass against a real Linux Obsidian extracted from the
upstream AppImage.

## What's inside

- `Dockerfile` - Debian 12 + Node 20 + Rust toolchain (with the
  `wasm32-unknown-unknown` target pre-installed) + Xvfb + xauth +
  Electron's GUI deps + `p7zip-full` for AppImage extraction.
- `docker-compose.yml` - convenience wrapper that bind-mounts the
  repo and named volumes for `node_modules`, the cargo cache, and
  the Obsidian download cache.
- `run-tests.sh` - the entrypoint script: builds the syncline
  binary release, builds the WASM plugin, then runs `wdio` under
  Xvfb.
- `.dockerignore` - keeps the build context to just the
  Dockerfile (the source tree is bind-mounted at runtime, so the
  build context never needs more).

## One-time setup

```bash
podman compose -f e2e/docker/docker-compose.yml build
```

(`docker compose -f e2e/docker/docker-compose.yml build` works
identically.)

First build downloads ~1.5 GB (Debian base + Rust toolchain + apt
packages) and pre-installs the `wasm32-unknown-unknown` Rust target
+ `wasm-pack`. Subsequent builds are cached.

On a Linux host you usually want the in-container user to match your
host UID so files written into the bind mount come out with the
right owner:

```bash
USER_UID=$(id -u) USER_GID=$(id -g) \
  podman compose -f e2e/docker/docker-compose.yml build
```

macOS / Windows hosts use the bind-mount file-server's translation
layer and can ignore the UID args.

## Running tests

Full suite:

```bash
podman compose -f e2e/docker/docker-compose.yml run --rm e2e
```

Single spec (the recommended dev loop):

```bash
podman compose -f e2e/docker/docker-compose.yml run --rm e2e \
  bash e2e/docker/run-tests.sh \
  --spec test/specs/plugin-cross-vault-install.e2e.ts
```

Drop into an interactive shell:

```bash
podman compose -f e2e/docker/docker-compose.yml run --rm e2e bash
```

Both specs produce `Running: chrome (v120.0.6099.283) on linux` -
real Obsidian, real chromedriver, headless via Xvfb.

## What the container does on each run

1. `cargo build --release --bin syncline` - incremental thanks to
   the persisted `target/` volume.
2. `obsidian-plugin/`: `npm ci` then `npm run build` (which calls
   `wasm-pack build` and rolls the plugin up into `main.js`).
3. `e2e/`: `npm ci` then `xvfb-run npx wdio run wdio.conf.ts`.

The first run downloads Obsidian itself (the AppImage matching the
test config's electron version) into `e2e/.obsidian-cache/`. That
directory is a named volume, so subsequent runs are fast.

## Why not run Obsidian on the host directly

Host-direct works fine on a developer macOS/Windows/Linux desktop,
but a CI runner usually has no display server, no Obsidian, and
possibly no Rust toolchain. The container removes those assumptions
and pins the Linux libraries Electron 28 wants (Chrome 120 era)
which otherwise drift between distros.

## Known caveats

- `shm_size` is bumped to 2 GB. The Electron 28 / Chromium 120
  stack inside Obsidian 1.5.x crashes with the Compose default of
  64 MB.
- `wdio-obsidian-service` automatically appends `--no-sandbox` on
  Linux, so the container does not need extra Chromium flags. We
  still run as a non-root `runner` user out of habit.
- The plugin and the Rust binary are built **inside** the container
  every run. That's deliberate: a CI image baked at build time
  would go stale within minutes of any source change, and bake-time
  builds hide the test's "does this actually rebuild from source"
  guarantee.
- `init: true` is set so the wdio + chromedriver + Obsidian
  subprocess tree gets reaped cleanly when a test container exits.
- **Apple Silicon hosts via Docker / Podman**: the
  `obsidian-versions.json` cache contains both arm64 and x64
  AppImages, but `node:20-bookworm` is multi-arch so an arm64 host
  may pull the arm64 base. If your host arch and the cached
  Obsidian arch ever disagree, force the platform:
  ```bash
  DOCKER_DEFAULT_PLATFORM=linux/amd64 \
    podman compose -f e2e/docker/docker-compose.yml run --rm e2e
  ```
- **SELinux hosts (Fedora/RHEL/CentOS)**: bind-mounts need a
  `:z` (or `:Z`) relabel. If the build inside the container fails
  with `Permission denied` on `/workspace`, edit the volume entry
  in `docker-compose.yml`:
  ```yaml
  volumes:
    - ../..:/workspace:z
  ```
