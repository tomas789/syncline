# Headless test image for the Obsidian wdio e2e suite.
#
# Layout:
#   - Debian 12 (bookworm) base for glibc compatibility with Obsidian
#     AppImages (which target a recent Ubuntu/glibc).
#   - Node.js 20 (matching the version the wdio service is exercised
#     against in CI).
#   - Rust toolchain so the syncline binary + the WASM plugin can be
#     built from source inside the container.
#   - Xvfb + Electron's GUI deps so real Obsidian can run headless.
#   - p7zip so wdio-obsidian-service can extract the Linux AppImage
#     without FUSE (the in-container path is `7z` -> extracted dir,
#     no `fusermount` involved).
#
# The image is a self-contained build environment - it does not bake
# in the source tree. The compose file mounts the repo into
# /workspace and runs the build + tests there. That keeps iteration
# fast (no rebuild on source changes) and lets the CI surface fail
# loudly when the on-disk artifacts diverge from the source tree.

FROM node:20-bookworm

ARG DEBIAN_FRONTEND=noninteractive

# Single apt install. The package list is the union of:
#   - build/utility: build-essential, pkg-config, libssl-dev, curl, ca-certificates,
#                    git, jq, sqlite3, wget
#   - AppImage extractor (no FUSE needed): p7zip-full
#   - Xvfb + helpers: xvfb, dbus-x11, x11-utils, xdg-utils
#   - Electron / Chromium 120 runtime: union of Puppeteer's "Chromium on Debian"
#     deps doc + Obsidian AppImage's ldd on Bookworm + libsecret-1-0
#     (Obsidian's safeStorage path).
# Avoid putting `# comments` inside the backslash continuation: they
# get joined into a single bash line and would silently swallow every
# package name after them.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config libssl-dev curl ca-certificates \
        git jq sqlite3 wget \
        p7zip-full \
        xvfb xauth dbus-x11 x11-utils xdg-utils \
        libgtk-3-0 libnss3 libxss1 libasound2 libgbm1 libdrm2 \
        libxcomposite1 libxdamage1 libxrandr2 libpangocairo-1.0-0 \
        libxshmfence1 libxkbcommon0 libxfixes3 libcurl4 libatspi2.0-0 \
        libcups2 libxcb-dri3-0 libsecret-1-0 libnspr4 libdbus-1-3 \
        libatk1.0-0 libatk-bridge2.0-0 libxext6 libxi6 libxtst6 \
        libuuid1 fonts-liberation libpango-1.0-0 \
    && rm -rf /var/lib/apt/lists/*

# Rust + wasm-pack at the system level so any user inside the
# container can use them without re-downloading. CARGO_HOME is split
# from RUSTUP_HOME so we can mount a writable $CARGO_HOME volume per
# user further down.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable --profile minimal \
    && rustup target add wasm32-unknown-unknown \
    && cargo install --locked wasm-pack \
    && chmod -R a+rX "$RUSTUP_HOME" "$CARGO_HOME"

# Non-root user. Electron sandbox requires either dropping privileges
# or `--no-sandbox` (which the wdio-obsidian-service already adds on
# Linux), and a non-root user keeps file mode bits sane on bind-
# mounted volumes from a Linux host. Mac/Windows hosts ignore uid.
#
# `node:20-bookworm` ships with `node` at UID/GID 1000. Strip it
# first so the new `runner` user can take that UID cleanly when the
# host requests it (the common Linux-host case).
ARG USER_UID=1000
ARG USER_GID=1000
RUN (getent passwd node >/dev/null && userdel -r node 2>/dev/null) || true \
    && (getent group node >/dev/null && groupdel node 2>/dev/null) || true \
    && groupadd -g "$USER_GID" runner \
    && useradd -m -u "$USER_UID" -g "$USER_GID" -s /bin/bash runner \
    && mkdir -p /workspace \
    && chown -R runner:runner /workspace /home/runner

USER runner
WORKDIR /workspace

# Per-user cargo cache so cargo install / target builds inside the
# container don't fight the system rustup install.
ENV CARGO_HOME=/home/runner/.cargo \
    PATH=/home/runner/.cargo/bin:/usr/local/cargo/bin:$PATH

# Default command runs the pipeline. Override with `bash` for an
# interactive shell, or pass extra arguments for wdio.
CMD ["bash", "/workspace/e2e/docker/run-tests.sh"]
