# Syncline v1 — Release Plan (Alpha Pre-release)

**Status:** Proposal — release/v1 branch
**Goal:** Ship Syncline v1 as a closed alpha to a small group of beta testers
without disturbing any existing v0 user, and without auto-updating any
existing install.

---

## 1. Who's affected today

Syncline is **not** registered in Obsidian's community plugin directory
(`obsidianmd/obsidian-releases/community-plugins.json` has no entry for
`syncline-obsidian` as of 2026-04-22). Every existing user therefore
falls into exactly one of three buckets:

1. **Manual install** — cloned the repo or copied files from a GitHub
   release by hand. Never auto-updates.
2. **BRAT users** — the majority of "real" testers. BRAT reads
   `manifest.json` from the GitHub release assets (since BRAT v1.1.0)
   and auto-upgrades by default to whatever the highest semver tag
   resolves to.
3. **Tom's own dev installs** — symlinked against the repo.

The risk we are managing: a BRAT user who added `Tomas789/syncline`
would, with no extra steps from us, auto-pull the first v1 release and
find themselves running an incompatible client against a v0 server.
That is the outcome to avoid.

---

## 2. The versioning decision

Use semver pre-release suffixes:

```
0.6.0           current stable (v0, on main)
0.6.1, 0.7.0    future v0 patch/minor releases on main (if any)
1.0.0-alpha.1   first v1 alpha
1.0.0-alpha.2   …
1.0.0-beta.1    feature-complete, unblocking real testers
1.0.0           public v1 (only after the alpha cohort signs off)
```

Why this works:

- **Semver precedence:** `1.0.0-alpha.1 < 1.0.0 < 1.0.1`. BRAT sorts by
  semver and will not auto-upgrade a `0.6.0` user to `1.0.0-alpha.1`
  unless they explicitly tracked the pre-release channel.
- **Obsidian built-in updater:** irrelevant here — the plugin is not
  in the registry. Even for users who eventually install from the
  registry, Obsidian's updater respects semver pre-release precedence.
- **Tag format:** use `1.0.0-alpha.1` verbatim as both the git tag
  (no `v` prefix — BRAT and Obsidian both expect the tag to equal the
  `manifest.json` version) and as the value in the released
  `manifest.json`.

### 2.1 Keep `main`'s `manifest.json` pinned to a v0 version

Do **not** commit `manifest.json` version `1.0.0-alpha.1` to `main`.
If we do, Obsidian's built-in updater (post-registry-submission) would
serve v1 to everyone as the stable. Keep `main`'s manifest at whatever
the last v0 release was; bump it only when v1 ships as `1.0.0` on
`main`.

This is standard BRAT-compatible practice: "Don't commit `manifest.json`
to your default branch yet — Obsidian picks up an update once the
manifest on the default branch changes."

---

## 3. GitHub release mechanics

For each alpha, the release must have:

| Field                    | Value                                                                 |
|--------------------------|-----------------------------------------------------------------------|
| Tag                      | `1.0.0-alpha.1` (on `release/v1`, **no `v` prefix**)                  |
| Release name             | `1.0.0-alpha.1`                                                       |
| "Set as a pre-release"   | ✅ **checked** — marks the release as a pre-release in GitHub's UI     |
| Target branch            | `release/v1`                                                          |
| Release body             | Changelog, known issues, how to revert (§5.3)                         |
| Assets                   | `main.js`, `manifest.json`, `styles.css` (BRAT reads manifest from the asset, not from the repo) |
| `manifest.json` contents | `"version": "1.0.0-alpha.1"` (must match the tag exactly)             |

The "pre-release" checkbox in the GitHub UI does **two** things we want:

1. GitHub's "Latest release" pointer keeps pointing at the last stable
   (`0.6.0`), so casual "download latest" users don't stumble into v1.
2. BRAT shows the release only to testers who explicitly opt into
   pre-release tracking for this repo. BRAT's default channel (stable)
   ignores it.

### 3.1 `manifest-beta.json` is obsolete

Don't add `manifest-beta.json`. Since BRAT v1.1.0 it is ignored —
BRAT now reads the `manifest.json` attached to each release asset
directly. Keeping a beta manifest alongside would be dead code and a
place for version drift.

---

## 4. Release-please vs v1

The repo already uses `release-please` (see
`.github/workflows/release-please.yml`), triggered on pushes to `main`.
v1 alphas must **not** go through release-please because:

- release-please would try to bump `manifest.json` on `main`, which
  violates §2.1.
- release-please builds its changelog from conventional commits on
  `main`; the v1 alpha's commits live on `release/v1`.

Two viable options, pick one:

### 4a. Manual releases from `release/v1` (recommended for alphas)

- Tag manually: `git tag 1.0.0-alpha.1 && git push origin 1.0.0-alpha.1`.
- Add a tag-triggered workflow that builds the plugin (`main.js`,
  `manifest.json`, `styles.css`) + Rust binaries and publishes a
  GitHub Release marked as pre-release.
- Once v1 is ready to become stable, merge `release/v1` into `main`
  and let release-please cut `1.0.0` normally.

### 4b. release-please with the `prerelease` branch feature

release-please v4 supports per-branch release configs, but setting it
up for a temporary pre-release channel is more config than it's worth
for a handful of alphas. Revisit only if we stay in alpha for many
months.

**Decision: go with 4a.** Add a new workflow
`.github/workflows/prerelease.yml` triggered on `push: tags:
'*-alpha.*'` and `'*-beta.*'` that:

1. Checks out the tag (from `release/v1`).
2. Builds `wasm/`, runs `npm run build`, collects the three plugin
   assets.
3. Builds Rust release binaries for the three platforms.
4. Calls `gh release create <tag> --prerelease --target release/v1
   --notes-file CHANGELOG_v1.md artifacts/*`.

Keep release-please on `main` exactly as it is.

---

## 5. Distribution strategy for the alpha cohort

### 5.1 Onboarding a tester

1. Tester installs BRAT (from the Obsidian community catalogue, one-time).
2. BRAT → "Add beta plugin" → `Tomas789/syncline`.
3. Enable "Enable beta updates" for this plugin in BRAT's settings so
   they get future alphas automatically.
4. Tester also runs a v1 server (separate binary, built from the same
   tag) — alphas are server+plugin pairs; protocol mismatch (§7 of
   `DESIGN_DOC_V1.md`) refuses mixed meshes.
5. Tester starts from a backed-up vault. See §5.3 for rollback.

### 5.2 What the tester sees

- First launch: plugin detects v0 state (`.syncline/` + `uuidMap.json`),
  runs local migration (DESIGN_DOC_V1.md §7), writes a
  `.syncline.v0.bak/` backup, initialises a v1 manifest.
- Subsequent launches: ordinary v1 sync.
- On disagreement with the server, `MSG_VERSION` handshake refuses
  connection — the plugin surfaces "server is v0; please upgrade" in
  the status bar instead of silently corrupting state.

### 5.3 Rollback path (must exist before we ship alpha.1)

Every alpha tester must be able to revert to v0 in under 5 minutes:

1. In BRAT, remove `Tomas789/syncline` from beta plugins.
2. Reinstall v0: BRAT → "Add plugin" → same repo, pick tag `0.6.0`
   (frozen version feature).
3. In the vault: delete `.syncline/`, restore `.syncline.v0.bak/`
   contents, reconnect to a v0 server.

The migration (§7 of `DESIGN_DOC_V1.md`) must produce
`.syncline.v0.bak/` before touching anything else — this is the
linchpin of the rollback and is already in the design. Do not ship
alpha.1 until the migration preserves `.v0.bak/`.

### 5.4 Tester communication channel

One channel (Discord / email thread / GitHub issue labelled
`alpha-feedback`). Each alpha release posts:

- What changed since the previous alpha.
- Known bugs not yet fixed.
- Explicit "if you see X, report it" call-outs.
- Rollback reminder.

---

## 6. Promotion criteria

### Alpha → Beta (`1.0.0-beta.1`)

All of:

1. The 21 e2e tests + 196 unit tests pass on `release/v1` (already met
   as of 2026-04-22).
2. At least 3 alpha testers have run a multi-day vault against a v1
   server with no reported data loss.
3. `test_migration_v0_to_v1_preserves_all_files` passes on a real
   user's exported v0 vault (not just synthetic fixtures).
4. BRAT install → migrate → uninstall → BRAT install v0 → vault intact.
   Rehearsed at least once per alpha round.

### Beta → Stable (`1.0.0` on `main`)

All of:

1. Two weeks of beta with zero P0 bugs.
2. Tom signs off on `release/v1` → `main` merge.
3. Registry submission PR (`obsidianmd/obsidian-releases`) can be
   prepared in parallel; not a blocker for `1.0.0` tag itself.

---

## 7. What to write down *before* alpha.1

A short checklist, to avoid surprises:

- [ ] `prerelease.yml` GitHub workflow committed on `release/v1` and
      tested on a throwaway tag (e.g. `0.0.1-test.1`, deleted after).
- [ ] `.github/workflows/release-please.yml` confirmed to only fire on
      `main` (it already does — lines 3–4 of that file).
- [ ] `docs/ALPHA_TESTER_GUIDE.md` drafted: installation, rollback,
      known bugs, how to report.
- [ ] `CHANGELOG_v1.md` seeded — release notes are built from this.
- [ ] Migration's `.syncline.v0.bak/` preservation verified end-to-end
      on a real vault (not just in unit tests).
- [ ] Tom picks the alpha cohort (3–5 testers max for alpha.1) and
      briefs them.

---

## 8. Non-goals for alpha

- No registry submission. Alpha runs entirely via BRAT.
- No Windows signing / notarisation work for the server binary; alpha
  testers who need Windows can build from source.
- No localisation, no onboarding wizard, no "import from Dropbox"
  niceties. Alpha is correctness-only.
- No backporting fixes to v0. Once v1 is alpha, v0 is
  maintenance-only: security patches only, no features.
