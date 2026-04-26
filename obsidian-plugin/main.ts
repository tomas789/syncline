import {
  App,
  Plugin,
  PluginSettingTab,
  Setting,
  Notice,
  TFile,
  TAbstractFile,
  debounce,
} from "obsidian";
// @ts-ignore - rollup base64-inlines the .wasm binary at build time.
import wasmBinary from "./wasm/syncline_bg.wasm";
import * as wasmModule from "./wasm/syncline.js";

// ---------------------------------------------------------------------------
// Settings & persistence layout
// ---------------------------------------------------------------------------

interface SynclineSettings {
  serverUrl: string;
  autoSync: boolean;
  /** Stable per-installation ActorId (UUIDv4, hyphenated). Minted on first run. */
  actorId: string | null;
}

const DEFAULT_SETTINGS: SynclineSettings = {
  serverUrl: "ws://localhost:3030/sync",
  autoSync: true,
  actorId: null,
};

type SyncStatus = "synced" | "syncing" | "error" | "disconnected";

// ---------------------------------------------------------------------------
// WASM binding shape (v1)
// ---------------------------------------------------------------------------

let wasmInitialized = false;
let wasm: WasmModule | null = null;

interface WasmModule {
  SynclineV1Client: new (
    url: string,
    actorIdHex: string | null | undefined,
  ) => SynclineV1Client;
}

interface ProjectionRow {
  id: string;
  path: string;
  kind: "text" | "binary" | "directory";
  blob_hash: string | null;
  size: number;
  is_conflict_copy: boolean;
}

interface SynclineV1Client {
  actorId(): string;
  initManifest(): void;
  loadManifestState(state: Uint8Array, lamport: number): void;
  manifestSnapshot(): Uint8Array;
  lamport(): bigint | number;
  onManifestChanged(cb: () => void): void;
  onContentChanged(cb: (nodeId: string) => void): void;
  onBlob(cb: (hash: string, data: Uint8Array) => void): void;
  onStatus(cb: (status: string) => void): void;
  connect(): void;
  disconnect(): void;
  isConnected(): boolean;
  createText(path: string, size: number): string;
  createTextAllowingCollision(path: string, size: number): string;
  createBinary(path: string, blobHash: string, size: number): string;
  delete(path: string): void;
  rename(from: string, to: string): void;
  recordModifyText(path: string): void;
  recordModifyBinary(path: string, blobHash: string, size: number): void;
  projectionJson(): string;
  projectionHashHex(): string;
  sendVerify(): void;
  subscribeContent(nodeIdHex: string, state?: Uint8Array | null): void;
  unsubscribeContent(nodeIdHex: string): void;
  getContentText(nodeIdHex: string): string | undefined;
  updateContentText(nodeIdHex: string, newContent: string): void;
  contentSnapshot(nodeIdHex: string): Uint8Array | undefined;
  sendBlob(bytes: Uint8Array): string;
  requestBlob(blobHashHex: string): void;
  free(): void;
}

async function initWasm(): Promise<WasmModule> {
  if (wasmInitialized && wasm) return wasm;
  const buffer = Uint8Array.from(atob(wasmBinary as unknown as string), (c) =>
    c.charCodeAt(0),
  );
  await wasmModule.default(Promise.resolve(buffer));
  wasmInitialized = true;
  wasm = wasmModule as unknown as WasmModule;
  console.debug("[Syncline] WASM initialized (v1)");
  return wasm;
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/**
 * How long we suppress file-watcher events after we write a remote update
 * to disk. Must exceed the onFileModify debounce window so our own writes
 * can't race back in as spurious local modifies. Same invariant as v0 —
 * see docs/tla/SynclineSyncDiffLayer.tla.
 */
const IGNORE_CHANGES_TIMEOUT_MS = 1000;

/** Extensions that project as text nodes (Y.Text CRDT). Everything else is binary. */
const TEXT_EXTENSIONS = new Set(["md", "txt"]);

function isTextFile(file: TFile): boolean {
  return TEXT_EXTENSIONS.has((file.extension ?? "").toLowerCase());
}

/** Web Crypto SHA-256 → lowercase hex. */
async function sha256Hex(data: ArrayBuffer): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", data);
  return Array.from(new Uint8Array(digest))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

/**
 * Obsidian's vault.create / vault.createFolder throw with a message that
 * contains "already exists" when the target is on disk. This happens when
 * the vault's in-memory file tree is desynced from disk (folders created
 * by an external process, by adapter.mkdir, or differing in case/Unicode).
 * Treat these as a no-op rather than rethrowing — the path is exactly
 * what we wanted.
 */
function isAlreadyExistsError(e: unknown): boolean {
  const msg = (e instanceof Error ? e.message : String(e)).toLowerCase();
  return msg.includes("already exists");
}

/**
 * "ENOENT: no such file or directory" — the inverse of the above. The
 * vault index disagrees with disk in the other direction: Obsidian
 * thinks a file/state-blob is there, the underlying syscall says no.
 * Surfaces during vault.read on a stale TFile, fileManager.trashFile
 * on a path that vanished, and adapter.remove racing with adapter.exists.
 * These are recoverable: the desired post-state ("file is gone" or
 * "content not loadable") matches reality, so treat as benign.
 */
function isMissingFileError(e: unknown): boolean {
  const msg = (e instanceof Error ? e.message : String(e));
  return /ENOENT|no such file or directory/i.test(msg);
}

/**
 * Transient WebSocket-not-yet-up errors thrown by the WASM client.
 * Reconcile fires before the WS handshake completes during cold sync,
 * and the same passes will re-fire once the server has replayed the
 * manifest, so "not connected" is benign here.
 */
function isNotConnectedError(e: unknown): boolean {
  const msg = (e instanceof Error ? e.message : String(e)).toLowerCase();
  return msg.includes("not connected");
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

export default class SynclinePlugin extends Plugin {
  settings!: SynclineSettings;
  statusBarItem!: HTMLElement;
  statusIcon!: HTMLElement;
  statusText!: HTMLElement;
  ribbonIconEl!: HTMLElement;
  syncStatus: SyncStatus = "disconnected";

  client: SynclineV1Client | null = null;
  /**
   * Per-event-kind suppression of vault echoes that follow our own writes.
   * Keyed by event type ("modify"|"create"|"delete"|"rename") then path.
   * Suppressing a `modify` echo on `foo.md` must not suppress a later
   * `rename` of the same path, so the kinds are tracked separately.
   */
  ignoreEvents: { modify: Set<string>; create: Set<string>; delete: Set<string>; rename: Set<string> } = {
    modify: new Set(),
    create: new Set(),
    delete: new Set(),
    rename: new Set(),
  };

  /** Last projection we reconciled against — keyed by path. */
  lastProjection: Map<string, ProjectionRow> = new Map();
  /** Node ids we've subscribed to a content subdoc for. */
  subscribedContent: Set<string> = new Set();
  /** Blob hashes we've already fetched this session (to avoid re-requests). */
  requestedBlobs: Set<string> = new Set();

  /**
   * Single-flight guard for reconcileProjection. The manifest CRDT can
   * fire `onManifestChanged` hundreds of times in a row (one per remote
   * update during a cold sync); each fire scheduled a fresh reconcile,
   * and concurrent reconciles raced on createFolder/create — flooding
   * the console with spurious "already exists" errors.
   *
   * - `runningReconcile` holds the in-flight reconcile (or null).
   * - `reconcilePending` collapses any further requests into one trailing
   *   run that fires after the current one settles.
   */
  private runningReconcile: Promise<void> | null = null;
  private reconcilePending: boolean = false;

  statusCheckInterval: number | null = null;
  reconnectTimeout: number | null = null;
  reconnectAttempts = 0;

  // ---------------------------------------------------------------
  // Lifecycle
  // ---------------------------------------------------------------

  async onload() {
    console.debug("[Syncline] Loading plugin (v1)…");
    await this.loadSettings();
    this.addSettingTab(new SynclineSettingTab(this.app, this));

    this.statusBarItem = this.addStatusBarItem();
    this.statusBarItem.addClass("syncline-status-bar");
    this.statusIcon = this.statusBarItem.createDiv({ cls: "status-icon disconnected" });
    this.statusText = this.statusBarItem.createDiv({ text: "Syncline: disconnected" });
    this.statusBarItem.onClickEvent(() => this.showStatusDetails());

    this.ribbonIconEl = this.addRibbonIcon("sync", "Syncline: disconnected", () =>
      this.showStatusDetails(),
    );
    this.ribbonIconEl.addClass("syncline-ribbon", "disconnected");

    this.registerEvent(this.app.vault.on("modify", this.onFileModify));
    this.registerEvent(this.app.vault.on("create", this.onFileCreate));
    this.registerEvent(this.app.vault.on("delete", this.onFileDelete));
    this.registerEvent(this.app.vault.on("rename", this.onFileRename));

    if (this.settings.autoSync) {
      await this.connect();
    }
  }

  onunload() {
    this.stopStatusCheck();
    if (this.reconnectTimeout !== null) {
      window.clearTimeout(this.reconnectTimeout);
      this.reconnectTimeout = null;
    }
    this.disconnect();
  }

  async loadSettings() {
    this.settings = Object.assign(
      {},
      DEFAULT_SETTINGS,
      await this.loadData(),
    ) as SynclineSettings;
  }

  async saveSettings() {
    await this.saveData(this.settings);
  }

  // ---------------------------------------------------------------
  // On-disk persistence for CRDT state
  // ---------------------------------------------------------------

  /** Root folder for v1 CRDT snapshots — separate from any v0 leftovers. */
  private get stateRoot(): string {
    return `${this.app.vault.configDir}/plugins/syncline-obsidian/v1`;
  }
  private get manifestPath(): string {
    return `${this.stateRoot}/manifest.bin`;
  }
  private get lamportPath(): string {
    return `${this.stateRoot}/lamport.txt`;
  }
  private contentPath(nodeId: string): string {
    return `${this.stateRoot}/content/${nodeId}.bin`;
  }

  private async ensureStateRoot(): Promise<void> {
    const adapter = this.app.vault.adapter;
    if (!(await adapter.exists(this.stateRoot))) {
      await adapter.mkdir(this.stateRoot);
    }
    const contentDir = `${this.stateRoot}/content`;
    if (!(await adapter.exists(contentDir))) {
      await adapter.mkdir(contentDir);
    }
  }

  private async loadManifestFromDisk(): Promise<{ state: Uint8Array; lamport: number } | null> {
    const adapter = this.app.vault.adapter;
    if (!(await adapter.exists(this.manifestPath))) return null;
    try {
      const state = new Uint8Array(await adapter.readBinary(this.manifestPath));
      let lamport = 0;
      if (await adapter.exists(this.lamportPath)) {
        const txt = await adapter.read(this.lamportPath);
        lamport = Number.parseInt(txt.trim(), 10) || 0;
      }
      return { state, lamport };
    } catch (e) {
      console.error("[Syncline] Failed to load manifest from disk:", e);
      return null;
    }
  }

  private async persistManifest(): Promise<void> {
    if (!this.client) return;
    try {
      await this.ensureStateRoot();
      const snap = this.client.manifestSnapshot();
      // Zero-length means uninitialised — don't clobber.
      if (!snap || snap.length === 0) return;
      const buffer = snap.buffer.slice(
        snap.byteOffset,
        snap.byteOffset + snap.byteLength,
      ) as ArrayBuffer;
      await this.app.vault.adapter.writeBinary(this.manifestPath, buffer);
      const lamport = String(this.client.lamport());
      await this.app.vault.adapter.write(this.lamportPath, lamport);
    } catch (e) {
      console.error("[Syncline] Failed to persist manifest:", e);
    }
  }

  private persistManifestDebounced = debounce(() => void this.persistManifest(), 500, false);

  private async loadContentState(nodeId: string): Promise<Uint8Array | null> {
    const adapter = this.app.vault.adapter;
    const p = this.contentPath(nodeId);
    if (!(await adapter.exists(p))) return null;
    try {
      return new Uint8Array(await adapter.readBinary(p));
    } catch (e) {
      // exists() returned true but read raced with a concurrent
      // removeContentState — treat as "no state" and move on.
      if (isMissingFileError(e)) return null;
      console.error(`[Syncline] Failed to load content state ${nodeId}:`, e);
      return null;
    }
  }

  private async persistContentState(nodeId: string): Promise<void> {
    if (!this.client) return;
    try {
      await this.ensureStateRoot();
      const snap = this.client.contentSnapshot(nodeId);
      if (!snap) return;
      const buffer = snap.buffer.slice(
        snap.byteOffset,
        snap.byteOffset + snap.byteLength,
      ) as ArrayBuffer;
      await this.app.vault.adapter.writeBinary(this.contentPath(nodeId), buffer);
    } catch (e) {
      console.error(`[Syncline] Failed to persist content ${nodeId}:`, e);
    }
  }

  private async removeContentState(nodeId: string): Promise<void> {
    const adapter = this.app.vault.adapter;
    const p = this.contentPath(nodeId);
    if (await adapter.exists(p)) {
      try {
        await adapter.remove(p);
      } catch (e) {
        // Concurrent remove already cleaned this up — that's fine.
        if (isMissingFileError(e)) return;
        console.error(`[Syncline] Failed to remove content state ${nodeId}:`, e);
      }
    }
  }

  // ---------------------------------------------------------------
  // Status / UI
  // ---------------------------------------------------------------

  updateStatus(status: SyncStatus, text?: string) {
    this.syncStatus = status;
    this.statusIcon.className = `status-icon ${status}`;
    const statusTexts: Record<SyncStatus, string> = {
      synced: "Syncline: synced",
      syncing: "Syncline: syncing…",
      error: "Syncline: error",
      disconnected: "Syncline: disconnected",
    };
    const newText = text ?? statusTexts[status];
    this.statusText.setText(newText);
    if (this.ribbonIconEl) {
      this.ribbonIconEl.removeClass("synced", "syncing", "error", "disconnected");
      this.ribbonIconEl.addClass(status);
      this.ribbonIconEl.setAttribute("aria-label", newText);
    }
  }

  showStatusDetails() {
    const count = this.lastProjection.size;
    const status =
      this.syncStatus === "synced"
        ? `Connected (${count} file${count === 1 ? "" : "s"})`
        : this.syncStatus === "syncing"
          ? "Connecting…"
          : this.syncStatus === "error"
            ? "Connection error — click to reconnect"
            : "Disconnected — click to connect";
    new Notice(`Syncline: ${status}`);
    if (this.syncStatus === "error" || this.syncStatus === "disconnected") {
      void this.connect();
    }
  }

  // ---------------------------------------------------------------
  // Connection lifecycle
  // ---------------------------------------------------------------

  async connect() {
    if (this.reconnectTimeout !== null) {
      window.clearTimeout(this.reconnectTimeout);
      this.reconnectTimeout = null;
    }

    try {
      this.updateStatus("syncing", "Syncline: connecting…");
      console.debug("[Syncline] Connecting to:", this.settings.serverUrl);

      const wasmMod = await initWasm();
      this.client = new wasmMod.SynclineV1Client(
        this.settings.serverUrl,
        this.settings.actorId ?? undefined,
      );

      // Persist the actor id the first time so we don't keep minting new ones.
      if (!this.settings.actorId) {
        this.settings.actorId = this.client.actorId();
        await this.saveSettings();
      }

      await this.ensureStateRoot();
      const persisted = await this.loadManifestFromDisk();
      if (persisted) {
        this.client.loadManifestState(persisted.state, persisted.lamport);
      } else {
        this.client.initManifest();
      }

      // The WASM observer fires synchronously while it still holds a
      // mutable borrow on the manifest RefCell. If we run reconcile
      // here, its synchronous `projectionJson()` re-enters the same
      // RefCell and the borrow check panic surfaces as a silent
      // exception, leaving `lastProjection` empty. Defer to a microtask
      // so the borrow is released before we read.
      this.client.onManifestChanged(() => {
        this.persistManifestDebounced();
        void Promise.resolve().then(() => this.reconcileProjection());
      });
      this.client.onContentChanged((nodeId) => {
        void Promise.resolve().then(() => this.onContentChanged(nodeId));
      });
      this.client.onBlob((hash, bytes) => {
        void this.onBlobReceived(hash, bytes);
      });
      this.client.onStatus((s) => {
        console.debug("[Syncline] status:", s);
      });

      this.client.connect();

      // Wait for the WS handshake to complete before scanning the local
      // vault or running the first reconcile. Without this, two races
      // fire on every restart with a populated cache:
      //
      //   * scanLocalVault sees an empty manifest (the server's STEP_2
      //     hasn't arrived yet) and re-ingests every existing vault
      //     file as a "new" manifest entry. When the server's STEP_2
      //     lands moments later, the merge produces a conflict copy
      //     of every file (#58).
      //
      //   * ingestNewFile fired synthetically during Obsidian's vault
      //     bootstrap hits the same empty live projection and falls
      //     through to createText, with the same conflict-copy
      //     fallout (#58, second surface).
      //
      // Polling `isConnected()` is the simplest signal — `onStatus`
      // fires with "connected" at the same time, so they're equivalent
      // observability-wise.
      await new Promise<void>((resolve, reject) => {
        const start = Date.now();
        const HANDSHAKE_TIMEOUT_MS = 30_000;
        const tick = () => {
          if (this.client?.isConnected()) return resolve();
          if (Date.now() - start > HANDSHAKE_TIMEOUT_MS) {
            return reject(new Error("WebSocket handshake timed out"));
          }
          window.setTimeout(tick, 50);
        };
        tick();
      });

      // Ingest any vault files not yet in the manifest (first run, or a
      // file created while offline). Done after the handshake completes,
      // so scanLocalVault below sees the server's full projection and
      // doesn't re-ingest paths the server already knows about.
      await this.scanLocalVault();
      await this.reconcileProjection();

      this.startStatusCheck();
    } catch (error) {
      console.error("[Syncline] Connection error:", error);
      this.updateStatus("error");
      this.scheduleReconnect();
    }
  }

  scheduleReconnect() {
    if (this.reconnectTimeout !== null) return;
    const delay = Math.min(1000 * Math.pow(2, this.reconnectAttempts), 30000);
    this.reconnectAttempts++;
    console.debug(`[Syncline] reconnect in ${delay}ms (attempt ${this.reconnectAttempts})`);
    this.reconnectTimeout = window.setTimeout(() => {
      this.reconnectTimeout = null;
      void this.connect();
    }, delay);
  }

  disconnect() {
    this.stopStatusCheck();
    if (this.reconnectTimeout !== null) {
      window.clearTimeout(this.reconnectTimeout);
      this.reconnectTimeout = null;
    }
    this.reconnectAttempts = 0;
    if (this.client) {
      this.client.disconnect();
      this.client.free();
      this.client = null;
    }
    for (const k of Object.values(this.ignoreEvents)) k.clear();
    this.lastProjection.clear();
    this.subscribedContent.clear();
    this.requestedBlobs.clear();
    this.reconcilePending = false;
    this.updateStatus("disconnected");
  }

  startStatusCheck() {
    if (this.statusCheckInterval !== null) {
      window.clearInterval(this.statusCheckInterval);
    }
    let wasConnected = false;
    let ticksDisconnected = 0;
    this.statusCheckInterval = window.setInterval(() => {
      if (!this.client) {
        this.updateStatus("disconnected");
        return;
      }
      if (this.client.isConnected()) {
        wasConnected = true;
        ticksDisconnected = 0;
        this.reconnectAttempts = 0;
        const count = this.lastProjection.size;
        this.updateStatus("synced", `Syncline: ${count} file${count === 1 ? "" : "s"}`);
      } else {
        if (wasConnected) {
          this.updateStatus("error", "Syncline: connection lost");
          wasConnected = false;
          ticksDisconnected = 0;
          this.scheduleReconnect();
        } else {
          ticksDisconnected++;
          if (ticksDisconnected > 5 && this.reconnectTimeout === null) {
            console.warn("[Syncline] Connection timed out after 5s");
            this.updateStatus("error", "Syncline: connection failed");
            this.scheduleReconnect();
          } else if (this.syncStatus !== "syncing" && this.syncStatus !== "error") {
            this.updateStatus("syncing", "Syncline: connecting…");
          }
        }
      }
    }, 1000);
  }

  stopStatusCheck() {
    if (this.statusCheckInterval !== null) {
      window.clearInterval(this.statusCheckInterval);
      this.statusCheckInterval = null;
    }
  }

  // ---------------------------------------------------------------
  // Local → manifest: scan-and-ingest on connect
  // ---------------------------------------------------------------

  /**
   * Ingest every vault file that isn't already represented in the
   * manifest. Uses `createTextAllowingCollision` / `createBinary` so a
   * remote manifest entry at the same path produces a projection
   * conflict (resolved by the Rust projection layer, §6.4) rather than
   * an error.
   */
  async scanLocalVault(): Promise<void> {
    if (!this.client) return;
    const projection = this.readProjection();
    // Case-insensitive lookup. On case-insensitive filesystems
    // (macOS APFS default, Windows NTFS default), two manifest paths
    // that differ only in case fold to the same disk file. The vault's
    // getFiles() returns whatever case macOS preserved, which can
    // differ from the manifest entry's case (e.g. when the manifest
    // came from a Linux peer). A byte-equal lookup misses the match
    // and re-uploads the file as a fresh manifest entry — silently
    // growing the manifest on every reconnect (#56). Lowercase is
    // good enough for ASCII; Unicode-NFC normalisation can layer on
    // top later if a non-ASCII case-only collision shows up.
    const pathsInManifest = new Set(projection.map((r) => r.path.toLowerCase()));
    for (const file of this.app.vault.getFiles()) {
      if (pathsInManifest.has(file.path.toLowerCase())) continue;
      try {
        if (isTextFile(file)) {
          const content = await this.app.vault.read(file);
          const size = new TextEncoder().encode(content).byteLength;
          const nodeId = this.client.createTextAllowingCollision(file.path, size);
          // knownState=null: this is a brand-new node, no prior `.bin`
          // can exist on disk. Passing null skips the loadContentState
          // await — keeping the sync `subscribedContent.add` and the
          // WASM `subscribeContent`/`updateContentText` in the same
          // synchronous turn so a manifest-observer-triggered reconcile
          // can't race-create an empty doc (#63).
          void this.subscribeToContent(nodeId, content, null);
        } else {
          const data = await this.app.vault.readBinary(file);
          const hash = await sha256Hex(data);
          const bytes = new Uint8Array(data);
          if (this.client.isConnected()) {
            this.client.sendBlob(bytes);
          }
          this.client.createBinary(file.path, hash, data.byteLength);
        }
      } catch (e) {
        console.error(`[Syncline] scan: failed to ingest ${file.path}:`, e);
      }
    }
  }

  // ---------------------------------------------------------------
  // Projection reconciliation — the core v1 idea.
  //
  // Whenever the manifest changes (either because we mutated it, or a
  // remote update arrived), re-project and make the vault match: create
  // missing files, rename moved files, delete tombstoned ones, and
  // subscribe/request any new content subdocs / blobs.
  // ---------------------------------------------------------------

  private readProjection(): ProjectionRow[] {
    if (!this.client) return [];
    try {
      return JSON.parse(this.client.projectionJson()) as ProjectionRow[];
    } catch (e) {
      console.error("[Syncline] bad projection JSON:", e);
      return [];
    }
  }

  /**
   * Reconcile the manifest projection with the on-disk vault. Single-
   * flighted: if a reconcile is already running when this is called,
   * mark a trailing reconcile and return; the trailing reconcile picks
   * up any state that landed during the in-flight one.
   */
  async reconcileProjection(): Promise<void> {
    if (this.runningReconcile) {
      this.reconcilePending = true;
      return this.runningReconcile;
    }
    const run = (async () => {
      try {
        do {
          this.reconcilePending = false;
          try {
            await this.reconcileProjectionInner();
          } catch (e) {
            // A throw here would strand the single-flight guard and
            // also drop any pending trailing reconcile. Log and keep
            // looping so subsequent manifest updates still get applied.
            console.error("[Syncline] reconcile error:", e);
          }
        } while (this.reconcilePending && this.client);
      } finally {
        this.runningReconcile = null;
      }
    })();
    this.runningReconcile = run;
    return run;
  }

  private async reconcileProjectionInner(): Promise<void> {
    if (!this.client) return;
    const projection = this.readProjection();
    const byPath = new Map<string, ProjectionRow>();
    const byId = new Map<string, ProjectionRow>();
    for (const row of projection) {
      byPath.set(row.path, row);
      byId.set(row.id, row);
    }

    // --- Removals: paths that were in the prior projection but aren't now ---
    for (const [path, prev] of this.lastProjection) {
      if (!byId.has(prev.id)) {
        await this.removeLocalFile(path);
        this.subscribedContent.delete(prev.id);
        void this.removeContentState(prev.id);
      } else {
        const current = byId.get(prev.id);
        if (current && current.path !== path) {
          await this.renameLocalFile(path, current.path);
        }
      }
    }

    // Publish the new projection BEFORE the additions loop. The loop
    // awaits per-file vault.create / ensureBinaryInSync, which yields
    // to the event loop and lets WS frames land. STEP_2 responses to
    // subscribes we fire here arrive mid-loop and dispatch through
    // onContentChanged, which calls findRowById against
    // this.lastProjection. If we leave the assignment until after the
    // loop, those callbacks miss every row whose subscribe completed
    // before the loop ended — silently dropping content for ~all files
    // on a real-world (~1k+ file) vault.
    this.lastProjection = byPath;

    // --- Additions / ensure-subscribed ---
    //
    // Pre-create unique parent dirs once (they're shared between many
    // siblings; 1k+ rows under nested dirs burns redundant ensureParent
    // lookups otherwise). Then iterate per-row serially.
    //
    // Tried Promise.all over the per-row loop; it was *slower* on real
    // vaults — desktop adapter contention + 1k concurrent setTimeouts
    // for ignoreEvents cleanup added more wall time than it saved.
    // The serial loop also yields between rows so STEP_2/blob frames
    // can land and be processed mid-walk, which is itself a win.
    const parentDirs = new Set<string>();
    for (const row of projection) {
      const parts = row.path.split("/");
      parts.pop();
      let cur = "";
      for (const part of parts) {
        cur = cur ? `${cur}/${part}` : part;
        parentDirs.add(cur);
      }
    }
    const sortedDirs = [...parentDirs].sort((a, b) => a.split("/").length - b.split("/").length);
    for (const dir of sortedDirs) {
      if (this.app.vault.getAbstractFileByPath(dir)) continue;
      try {
        await this.app.vault.createFolder(dir);
      } catch (e) {
        if (!isAlreadyExistsError(e)) console.error(`[Syncline] mkdir ${dir}:`, e);
      }
    }

    for (const row of projection) {
      const existing = this.app.vault.getAbstractFileByPath(row.path);
      if (row.kind === "text") {
        if (!(existing instanceof TFile)) {
          try {
            this.ignoreEvents.create.add(row.path);
            await this.app.vault.create(row.path, "");
          } catch (e) {
            if (!isAlreadyExistsError(e)) {
              console.error(`[Syncline] create placeholder ${row.path}:`, e);
            }
          } finally {
            setTimeout(() => this.ignoreEvents.create.delete(row.path), IGNORE_CHANGES_TIMEOUT_MS);
          }
        }
        if (!this.subscribedContent.has(row.id)) {
          void this.subscribeToContent(row.id);
        }
      } else if (row.kind === "binary") {
        if (row.blob_hash) {
          await this.ensureBinaryInSync(row);
        }
      }
    }
  }

  private async removeLocalFile(path: string): Promise<void> {
    const file = this.app.vault.getAbstractFileByPath(path);
    if (!(file instanceof TFile)) return;
    this.ignoreEvents.delete.add(path);
    try {
      await this.app.fileManager.trashFile(file);
    } catch (e) {
      // Disk doesn't have it — Obsidian's index will catch up. The
      // CRDT-side removal already happened, so this is a no-op locally.
      if (isMissingFileError(e)) return;
      console.error(`[Syncline] trash ${path}:`, e);
    } finally {
      setTimeout(() => this.ignoreEvents.delete.delete(path), IGNORE_CHANGES_TIMEOUT_MS);
    }
  }

  private async renameLocalFile(from: string, to: string): Promise<void> {
    const file = this.app.vault.getAbstractFileByPath(from);
    if (!(file instanceof TFile)) return;
    try {
      await this.ensureParentFolders(to);
      this.ignoreEvents.rename.add(from);
      this.ignoreEvents.rename.add(to);
      await this.app.fileManager.renameFile(file, to);
    } catch (e) {
      // Source no longer on disk (Obsidian's index hadn't caught up) —
      // the rename target either already exists or doesn't matter.
      if (isMissingFileError(e)) return;
      // Target already exists / is a folder collision — also benign,
      // we'll converge on the next reconcile pass.
      if (isAlreadyExistsError(e)) return;
      console.error(`[Syncline] rename ${from} → ${to}:`, e);
    } finally {
      setTimeout(() => {
        this.ignoreEvents.rename.delete(from);
        this.ignoreEvents.rename.delete(to);
      }, IGNORE_CHANGES_TIMEOUT_MS);
    }
  }

  private async ensureParentFolders(filePath: string): Promise<void> {
    const parts = filePath.split("/");
    parts.pop();
    let cur = "";
    for (const part of parts) {
      cur = cur ? `${cur}/${part}` : part;
      if (this.app.vault.getAbstractFileByPath(cur)) continue;
      // The in-memory file tree may be out of sync with disk (e.g. folder
      // created externally, by adapter.mkdir, or differing in
      // case/Unicode). Fall back to a direct adapter.exists() probe before
      // attempting createFolder so we don't spam errors on every reconcile.
      try {
        if (await this.app.vault.adapter.exists(cur)) continue;
      } catch {
        // adapter.exists itself shouldn't throw, but if it does we'll
        // still try createFolder below.
      }
      try {
        await this.app.vault.createFolder(cur);
      } catch (e) {
        if (isAlreadyExistsError(e)) continue;
        throw e;
      }
    }
  }

  // ---------------------------------------------------------------
  // Text content subdocs
  // ---------------------------------------------------------------

  /**
   * Subscribe to the content subdoc for `nodeId`. If `initialContent`
   * is given (from the local-file scanner / ingestNewFile), seed the
   * subdoc so our first update carries the file's bytes.
   *
   * Adds to `subscribedContent` SYNCHRONOUSLY before any await so a
   * concurrent caller (e.g. a reconcile triggered by the manifest
   * observer that fired in createText) sees the membership and bails.
   * Without that guard, two IIFEs race on `subscribeContent` — the
   * second call replaces the freshly-seeded WASM doc with an empty
   * one, dropping `initialContent` on the floor (#63).
   */
  private async subscribeToContent(nodeId: string, initialContent?: string, knownState?: Uint8Array | null): Promise<void> {
    if (!this.client) return;
    if (this.subscribedContent.has(nodeId)) return;
    this.subscribedContent.add(nodeId);
    let state = knownState;
    if (state === undefined) {
      state = await this.loadContentState(nodeId);
    }
    if (!this.client) return;
    try {
      this.client.subscribeContent(nodeId, state ?? null);
    } catch (e) {
      console.error(`[Syncline] subscribe_content ${nodeId}:`, e);
      this.subscribedContent.delete(nodeId);
      return;
    }
    if (initialContent !== undefined) {
      const current = this.client.getContentText(nodeId) ?? "";
      if (current !== initialContent) {
        this.client.updateContentText(nodeId, initialContent);
      }
    }
  }

  private async onContentChanged(nodeId: string): Promise<void> {
    if (!this.client) return;
    const row = this.findRowById(nodeId);
    if (!row || row.kind !== "text") return;
    const text = this.client.getContentText(nodeId);
    if (text == null) return;
    const file = this.app.vault.getAbstractFileByPath(row.path);
    void this.persistContentState(nodeId);
    if (file instanceof TFile) {
      try {
        let current: string;
        try {
          current = await this.app.vault.read(file);
        } catch (e) {
          if (!isMissingFileError(e)) throw e;
          // Vault index has the TFile but the underlying file was
          // deleted out from under us. Treat it as "needs to be (re-)
          // written" — fall through to write the CRDT content via the
          // adapter so the file is restored on disk.
          this.ignoreEvents.modify.add(row.path);
          await this.ensureParentFolders(row.path);
          await this.app.vault.adapter.write(row.path, text);
          return;
        }
        if (current === text) return;
        this.ignoreEvents.modify.add(row.path);
        await this.app.vault.modify(file, text);
      } catch (e) {
        console.error(`[Syncline] modify ${row.path}:`, e);
      } finally {
        setTimeout(() => this.ignoreEvents.modify.delete(row.path), IGNORE_CHANGES_TIMEOUT_MS);
      }
    } else {
      try {
        await this.ensureParentFolders(row.path);
        this.ignoreEvents.create.add(row.path);
        try {
          await this.app.vault.create(row.path, text);
        } catch (e) {
          if (!isAlreadyExistsError(e)) throw e;
          // The file is on disk but Obsidian's in-memory tree didn't see
          // it. We still need the content written; fall back to the
          // adapter so we don't silently drop the update. Obsidian's
          // file-watcher will reconcile its tree shortly after.
          await this.app.vault.adapter.write(row.path, text);
        }
      } catch (e) {
        console.error(`[Syncline] create ${row.path}:`, e);
      } finally {
        setTimeout(() => this.ignoreEvents.create.delete(row.path), IGNORE_CHANGES_TIMEOUT_MS);
      }
    }
  }

  private findRowById(nodeId: string): ProjectionRow | undefined {
    for (const row of this.lastProjection.values()) {
      if (row.id === nodeId) return row;
    }
    return undefined;
  }

  // ---------------------------------------------------------------
  // Binary blob protocol
  // ---------------------------------------------------------------

  /**
   * Make sure the file at `row.path` has content hashing to
   * `row.blob_hash`. If not, either push our bytes (remote has no blob
   * yet) or request the remote blob (we have different bytes).
   */
  private async ensureBinaryInSync(row: ProjectionRow): Promise<void> {
    if (!this.client || !row.blob_hash) return;
    const file = this.app.vault.getAbstractFileByPath(row.path);
    if (file instanceof TFile) {
      try {
        const data = await this.app.vault.readBinary(file);
        const localHash = await sha256Hex(data);
        if (localHash === row.blob_hash) return;
        this.tryRequestBlob(row.blob_hash);
      } catch (e) {
        // Disk-side I/O on a stale TFile — nothing to do.
        if (isMissingFileError(e)) return;
        console.error(`[Syncline] ensureBinaryInSync ${row.path}:`, e);
      }
    } else {
      // File missing on disk — request unconditionally. tryRequestBlob
      // dedupes per-hash, but for the missing-file branch that's wrong:
      // a previous request for this hash only wrote to whichever rows
      // happened to be in lastProjection when the blob landed. Rows
      // arriving in later manifest batches need their own delivery.
      // Server still has the bytes; re-request is cheap.
      if (!this.client.isConnected()) return;
      try {
        this.client.requestBlob(row.blob_hash);
        this.requestedBlobs.add(row.blob_hash);
      } catch (e) {
        if (isNotConnectedError(e)) return;
        console.error(`[Syncline] requestBlob ${row.blob_hash}:`, e);
      }
    }
  }

  /**
   * Request a blob from the server, but only if the WebSocket is
   * actually connected. The reconcile loop runs eagerly after the
   * manifest is loaded from disk — well before the WS handshake
   * completes — so a naive call throws "request_blob: not connected"
   * for every binary row. We must NOT mark the hash as requested in
   * that case, otherwise the post-connect reconcile pass skips it.
   * "not connected" is a transient state; the next reconcile (which
   * fires once the server replays the manifest after handshake) picks
   * it up.
   */
  private tryRequestBlob(hash: string): void {
    if (!this.client) return;
    if (this.requestedBlobs.has(hash)) return;
    if (!this.client.isConnected()) return;
    try {
      this.client.requestBlob(hash);
      this.requestedBlobs.add(hash);
    } catch (e) {
      // The connect state may have flipped between isConnected() and
      // requestBlob; treat as transient so the next reconcile retries.
      if (isNotConnectedError(e)) return;
      console.error(`[Syncline] requestBlob ${hash}:`, e);
    }
  }

  private async onBlobReceived(hash: string, bytes: Uint8Array): Promise<void> {
    // A blob may satisfy multiple projection rows (e.g. a conflict copy).
    const matches = Array.from(this.lastProjection.values()).filter(
      (r) => r.kind === "binary" && r.blob_hash === hash,
    );
    for (const row of matches) {
      const buffer = bytes.buffer.slice(
        bytes.byteOffset,
        bytes.byteOffset + bytes.byteLength,
      ) as ArrayBuffer;
      const file = this.app.vault.getAbstractFileByPath(row.path);
      const kind: 'modify' | 'create' = file instanceof TFile ? 'modify' : 'create';
      this.ignoreEvents[kind].add(row.path);
      try {
        if (file instanceof TFile) {
          await this.app.vault.modifyBinary(file, buffer);
        } else {
          await this.ensureParentFolders(row.path);
          try {
            await this.app.vault.createBinary(row.path, buffer);
          } catch (e) {
            if (!isAlreadyExistsError(e)) throw e;
            // Disk has the file even though the vault index doesn't —
            // write through the adapter so we don't drop the blob.
            await this.app.vault.adapter.writeBinary(row.path, buffer);
          }
        }
      } catch (e) {
        console.error(`[Syncline] write blob → ${row.path}:`, e);
      } finally {
        setTimeout(() => this.ignoreEvents[kind].delete(row.path), IGNORE_CHANGES_TIMEOUT_MS);
      }
    }
  }

  // ---------------------------------------------------------------
  // Vault → manifest: local file events
  // ---------------------------------------------------------------

  onFileModify = debounce(async (file: TAbstractFile) => {
    if (!(file instanceof TFile)) return;
    if (this.ignoreEvents.modify.has(file.path)) return;
    if (!this.client) return;

    const row = this.lastProjection.get(file.path);
    if (!row) {
      // Not yet in the projection — treat as a create event.
      void this.ingestNewFile(file);
      return;
    }

    if (row.kind === "text") {
      try {
        const content = await this.app.vault.read(file);
        if (this.ignoreEvents.modify.has(file.path)) return;
        const crdtContent = this.client.getContentText(row.id) ?? "";
        if (crdtContent === content) return;
        this.client.updateContentText(row.id, content);
        this.client.recordModifyText(row.path);
        void this.persistContentState(row.id);
      } catch (e) {
        // Stale TFile in the index for a file that's no longer on disk
        // — Obsidian will fire a delete event soon; nothing to do here.
        if (isMissingFileError(e)) return;
        console.error(`[Syncline] onModify text ${file.path}:`, e);
      }
    } else if (row.kind === "binary") {
      try {
        const data = await this.app.vault.readBinary(file);
        if (this.ignoreEvents.modify.has(file.path)) return;
        const hash = await sha256Hex(data);
        if (row.blob_hash === hash) return;
        this.client.sendBlob(new Uint8Array(data));
        this.client.recordModifyBinary(row.path, hash, data.byteLength);
      } catch (e) {
        if (isMissingFileError(e)) return;
        console.error(`[Syncline] onModify binary ${file.path}:`, e);
      }
    }
  }, 300);

  onFileCreate = (file: TAbstractFile) => {
    if (!(file instanceof TFile)) return;
    if (this.ignoreEvents.create.has(file.path)) return;
    void this.ingestNewFile(file);
  };

  private async ingestNewFile(file: TFile): Promise<void> {
    if (!this.client) return;
    // The path is already managed by the manifest (e.g. Obsidian fired
    // a synthetic create event during initial vault indexing for files
    // that were already on disk). The reconcile loop will pick up any
    // content drift via ensureBinaryInSync / onContentChanged — nothing
    // to do here.
    //
    // Use the LIVE projection (snapshot from the WASM client) rather
    // than `lastProjection`. `lastProjection` lags behind the live
    // manifest by one reconcile cycle, and during the first reconcile
    // after a cache wipe + reopen, every synthetic onFileCreate fires
    // before reconcile populates `lastProjection` — falling through to
    // `createText` for paths the LIVE projection already has, which
    // mints conflict copies of every existing file (#58).
    //
    // Comparison is case-insensitive (lowercase) so a case-folded disk
    // path (e.g. macOS preserved `CaseTest/` for a manifest entry at
    // `casetest/`) doesn't fall through to createText (#56).
    const live = this.readProjection();
    const filePathLower = file.path.toLowerCase();
    if (live.some((r) => r.path.toLowerCase() === filePathLower)) return;
    try {
      if (isTextFile(file)) {
        const content = await this.app.vault.read(file);
        const size = new TextEncoder().encode(content).byteLength;
        let nodeId: string;
        try {
          nodeId = this.client.createText(file.path, size);
        } catch (e) {
          // Path taken — fall back to the collision-tolerant variant; the
          // projection will resolve it with a `.conflict-` suffix.
          console.debug(`[Syncline] createText collision for ${file.path}:`, e);
          nodeId = this.client.createTextAllowingCollision(file.path, size);
        }
        // knownState=null — see comment in scanLocalVault for why.
        void this.subscribeToContent(nodeId, content, null);
      } else {
        const data = await this.app.vault.readBinary(file);
        const hash = await sha256Hex(data);
        this.client.sendBlob(new Uint8Array(data));
        try {
          this.client.createBinary(file.path, hash, data.byteLength);
        } catch (e) {
          // Path race: between the lastProjection check and now, the
          // manifest sprouted an entry at this path (likely a remote
          // update arrived mid-ingest). The blob we just sent is still
          // useful — the existing manifest entry's blob_hash will pull
          // it down on the next reconcile if hashes match. Nothing more
          // to do.
          console.debug(`[Syncline] createBinary collision for ${file.path}:`, e);
        }
      }
    } catch (e) {
      console.error(`[Syncline] ingestNewFile ${file.path}:`, e);
    }
  }

  onFileDelete = (file: TAbstractFile) => {
    if (!(file instanceof TFile)) return;
    if (this.ignoreEvents.delete.has(file.path)) return;
    if (!this.client) return;
    const row = this.lastProjection.get(file.path);
    if (!row) return;
    try {
      this.client.delete(file.path);
      this.subscribedContent.delete(row.id);
      void this.removeContentState(row.id);
    } catch (e) {
      console.error(`[Syncline] delete ${file.path}:`, e);
    }
  };

  onFileRename = (file: TAbstractFile, oldPath: string) => {
    if (!(file instanceof TFile)) return;
    if (this.ignoreEvents.rename.has(oldPath) || this.ignoreEvents.rename.has(file.path)) return;
    if (!this.client) return;
    const row = this.lastProjection.get(oldPath);
    if (row) {
      try {
        this.client.rename(oldPath, file.path);
      } catch (e) {
        console.error(`[Syncline] rename ${oldPath} → ${file.path}:`, e);
      }
    } else {
      void this.ingestNewFile(file);
    }
  };
}

// ---------------------------------------------------------------------------
// Settings UI
// ---------------------------------------------------------------------------

class SynclineSettingTab extends PluginSettingTab {
  plugin: SynclinePlugin;

  constructor(app: App, plugin: SynclinePlugin) {
    super(app, plugin);
    this.plugin = plugin;
  }

  display(): void {
    const { containerEl } = this;
    containerEl.empty();

    new Setting(containerEl).setName("Connection").setHeading();

    new Setting(containerEl)
      .setName("Server address")
      .setDesc("WebSocket URL (e.g., ws://localhost:3030/sync)")
      .addText((text) =>
        text
          .setPlaceholder("Enter server address")
          .setValue(this.plugin.settings.serverUrl)
          .onChange(async (value) => {
            this.plugin.settings.serverUrl = value;
            await this.plugin.saveSettings();
          }),
      );

    new Setting(containerEl)
      .setName("Auto sync")
      .setDesc("Sync automatically when Obsidian starts")
      .addToggle((toggle) =>
        toggle.setValue(this.plugin.settings.autoSync).onChange(async (value) => {
          this.plugin.settings.autoSync = value;
          await this.plugin.saveSettings();
        }),
      );

    new Setting(containerEl)
      .setName("Reconnect")
      .setDesc("Reconnect to the server")
      .addButton((button) =>
        button.setButtonText("Reconnect").onClick(() => {
          this.plugin.disconnect();
          void this.plugin.connect();
        }),
      );

    new Setting(containerEl).setName("Identity").setHeading();
    new Setting(containerEl)
      .setName("Actor ID")
      .setDesc("Stable per-installation identifier used for sync authorship.")
      .addText((text) => {
        text
          .setPlaceholder("(minted on first connect)")
          .setValue(this.plugin.settings.actorId ?? "")
          .setDisabled(true);
      });
  }
}
