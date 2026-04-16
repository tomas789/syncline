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
// @ts-ignore
import wasmBinary from "./wasm/syncline_bg.wasm";
import * as wasmModule from "./wasm/syncline.js";

interface SynclineSettings {
  serverUrl: string;
  autoSync: boolean;
  /** UUIDs of all files this client has ever seen (used for offline-deletion detection). */
  knownFiles: string[];
  /** Maps relative file path → permanent UUID. Persisted so renames survive restarts. */
  uuidMap: Record<string, string>;
}

const DEFAULT_SETTINGS: SynclineSettings = {
  serverUrl: "ws://localhost:3030/sync",
  autoSync: true,
  knownFiles: [],
  uuidMap: {},
};

type SyncStatus = "synced" | "syncing" | "error" | "disconnected";

let wasmInitialized = false;
let wasm: WasmModule | null = null;

interface WasmModule {
  SynclineClient: new (url: string) => SynclineClient;
}

interface SynclineClient {
  connect(): void;
  add_doc(doc_id: string, callback: () => void): void;
  add_doc_with_state(doc_id: string, state: Uint8Array, callback: () => void): void;
  add_binary_doc(
    doc_id: string,
    callback: () => void,
    blob_callback: (doc_id: string, data: Uint8Array) => void,
  ): void;
  remove_doc(doc_id: string): void;
  get_text(doc_id: string): string | undefined;
  get_doc_state(doc_id: string): Uint8Array | undefined;
  update(doc_id: string, content: string): void;
  set_text(doc_id: string, content: string): void;
  is_connected(): boolean;
  doc_count(): number;
  create_index(callback?: () => void): void;
  index_insert(key: string): void;
  index_remove(key: string): void;
  index_keys(): string[];
  get_meta_path(doc_id: string): string | undefined;
  set_meta_path(doc_id: string, path: string): void;
  get_meta_type(doc_id: string): string | undefined;
  set_meta_type(doc_id: string, file_type: string): void;
  get_blob_hash(doc_id: string): string | undefined;
  set_blob_hash(doc_id: string, hash: string): void;
  send_blob(doc_id: string, data: Uint8Array): void;
  request_blob(doc_id: string, hash: string): void;
  disconnect(): void;
  free(): void;
}

async function initWasm(): Promise<WasmModule> {
  if (wasmInitialized && wasm) {
    return wasm;
  }

  const buffer = Uint8Array.from(atob(wasmBinary as unknown as string), (c) =>
    c.charCodeAt(0),
  );
  await wasmModule.default(Promise.resolve(buffer));
  wasmInitialized = true;
  wasm = wasmModule as unknown as WasmModule;
  console.debug("[Syncline] WASM initialized");
  return wasm;
}

/**
 * Duration to suppress the file-watcher after writing a remote update to disk.
 * Must be strictly greater than the onFileModify debounce window (300 ms) to
 * guarantee that any debounced handler triggered by our own vault.modify()
 * call is still suppressed when it finally fires.
 *
 * See docs/tla/SynclineSyncDiffLayer.tla — the TLA+ model proved that
 * without this guard, a stale disk read generates spurious DELETE operations.
 */
const IGNORE_CHANGES_TIMEOUT_MS = 1000;

/** Text-based file extensions that use Y.Text CRDT for content sync. */
const TEXT_EXTENSIONS = ["md", "txt"];

/** Returns true if a file should be synced as binary (not via Y.Text CRDT). */
function isBinaryFile(file: TFile): boolean {
  return !TEXT_EXTENSIONS.includes(file.extension ?? "");
}

/** Compute SHA-256 hex hash of a byte array using Web Crypto. */
async function sha256Hex(data: ArrayBuffer): Promise<string> {
  const hashBuffer = await crypto.subtle.digest("SHA-256", data);
  const hashArray = Array.from(new Uint8Array(hashBuffer));
  return hashArray.map((b) => b.toString(16).padStart(2, "0")).join("");
}

export default class SynclinePlugin extends Plugin {
  settings: SynclineSettings;
  statusBarItem: HTMLElement;
  statusIcon: HTMLElement;
  statusText: HTMLElement;
  ribbonIconEl: HTMLElement;
  syncStatus: SyncStatus = "disconnected";

  client: SynclineClient | null = null;
  ignoreChanges: Set<string> = new Set();
  statusCheckInterval: number | null = null;
  /** Tracks UUID strings (not paths) for all files currently known to be synced. */
  knownFiles: Set<string> = new Set();
  reconnectTimeout: number | null = null;
  reconnectAttempts: number = 0;

  async onload() {
    console.debug("[Syncline] Loading plugin...");
    await this.loadSettings();
    this.addSettingTab(new SynclineSettingTab(this.app, this));

    this.statusBarItem = this.addStatusBarItem();
    this.statusBarItem.addClass("syncline-status-bar");
    this.statusIcon = this.statusBarItem.createDiv({
      cls: "status-icon disconnected",
    });
    this.statusText = this.statusBarItem.createDiv({
      text: "Syncline: disconnected",
    });

    this.statusBarItem.onClickEvent(() => {
      this.showStatusDetails();
    });

    this.ribbonIconEl = this.addRibbonIcon("sync", "Syncline: disconnected", () => {
      this.showStatusDetails();
    });
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
    // Ensure uuidMap exists on older saved data
    if (!this.settings.uuidMap) {
      this.settings.uuidMap = {};
    }
  }

  async saveSettings() {
    await this.saveData(this.settings);
  }

  // ---------------------------------------------------------------------------
  // UUID helpers
  // ---------------------------------------------------------------------------

  /** Returns the stable UUID for a file path, generating one if it doesn't exist yet. */
  getOrCreateUuid(filePath: string): string {
    if (!this.settings.uuidMap[filePath]) {
      this.settings.uuidMap[filePath] = crypto.randomUUID();
    }
    return this.settings.uuidMap[filePath];
  }

  /** Returns a uuid→path lookup derived from the current uuidMap. */
  reverseUuidMap(): Record<string, string> {
    return Object.fromEntries(
      Object.entries(this.settings.uuidMap).map(([path, uuid]) => [uuid, path]),
    );
  }

  // ---------------------------------------------------------------------------
  // CRDT state persistence
  // ---------------------------------------------------------------------------

  /** Directory inside the plugin config folder where CRDT states are stored. */
  private get crdtDir(): string {
    return `${this.app.vault.configDir}/plugins/syncline-obsidian/crdt`;
  }

  /** Persist the full CRDT state for a document so offline edits survive restarts. */
  private async saveCrdtState(uuid: string): Promise<void> {
    if (!this.client) return;
    const state = this.client.get_doc_state(uuid);
    if (!state) return;
    const path = `${this.crdtDir}/${uuid}.bin`;
    try {
      // Ensure the crdt directory exists
      if (!(await this.app.vault.adapter.exists(this.crdtDir))) {
        await this.app.vault.adapter.mkdir(this.crdtDir);
      }
      await this.app.vault.adapter.writeBinary(path, state.buffer);
    } catch (error) {
      console.error(`[Syncline] Error saving CRDT state for ${uuid}:`, error);
    }
  }

  /** Load persisted CRDT state for a document, or null if none exists. */
  private async loadCrdtState(uuid: string): Promise<Uint8Array | null> {
    const path = `${this.crdtDir}/${uuid}.bin`;
    try {
      if (await this.app.vault.adapter.exists(path)) {
        const buffer = await this.app.vault.adapter.readBinary(path);
        return new Uint8Array(buffer);
      }
    } catch (error) {
      console.error(`[Syncline] Error loading CRDT state for ${uuid}:`, error);
    }
    return null;
  }

  // ---------------------------------------------------------------------------
  // Status / UI
  // ---------------------------------------------------------------------------

  updateStatus(status: SyncStatus, text?: string) {
    this.syncStatus = status;
    this.statusIcon.className = `status-icon ${status}`;

    const statusTexts: Record<SyncStatus, string> = {
      synced: "Syncline: synced",
      syncing: "Syncline: syncing…",
      error: "Syncline: error",
      disconnected: "Syncline: disconnected",
    };

    const newText = text || statusTexts[status];
    this.statusText.setText(newText);

    if (this.ribbonIconEl) {
      this.ribbonIconEl.removeClass("synced", "syncing", "error", "disconnected");
      this.ribbonIconEl.addClass(status);
      this.ribbonIconEl.setAttribute("aria-label", newText);
    }
  }

  showStatusDetails() {
    const count = this.client?.doc_count() ?? 0;
    const status =
      this.syncStatus === "synced"
        ? `Connected (${count} files)`
        : this.syncStatus === "syncing"
          ? "Connecting..."
          : this.syncStatus === "error"
            ? "Connection error - click to reconnect"
            : "Disconnected - click to connect";

    new Notice(`Syncline: ${status}`);

    if (this.syncStatus === "error" || this.syncStatus === "disconnected") {
      void this.connect();
    }
  }

  // ---------------------------------------------------------------------------
  // Connection lifecycle
  // ---------------------------------------------------------------------------

  async connect() {
    if (this.reconnectTimeout !== null) {
      window.clearTimeout(this.reconnectTimeout);
      this.reconnectTimeout = null;
    }

    try {
      this.updateStatus("syncing", "Syncline: connecting…");
      console.debug("[Syncline] Connecting to:", this.settings.serverUrl);

      const wasmMod = await initWasm();
      this.client = new wasmMod.SynclineClient(this.settings.serverUrl);

      // Create the __index__ document first
      this.client.create_index(() => {
        this.onIndexUpdate();
      });

      // Register all vault files under their UUIDs BEFORE connecting
      const files = this.app.vault.getFiles();
      console.debug("[Syncline] Adding", files.length, "files to index");

      for (const file of files) {
        const uuid = this.getOrCreateUuid(file.path);
        this.knownFiles.add(uuid);
        this.client.index_insert(uuid);
      }

      // Open WebSocket — WASM will send SyncStep1 for all registered docs on open
      this.client.connect();

      // Subscribe to content for each file
      for (const file of files) {
        const uuid = this.settings.uuidMap[file.path];
        if (uuid) {
          if (isBinaryFile(file)) {
            this.addBinaryDocOnly(file, uuid);
          } else {
            void this.addDocOnly(file, uuid);
          }
        }
      }

      // Persist the (possibly expanded) uuidMap
      await this.saveSettings();

      this.startStatusCheck();
    } catch (error) {
      console.error("[Syncline] Connection error:", error);
      this.updateStatus("error");
      this.scheduleReconnect();
    }
  }

  scheduleReconnect() {
    if (this.reconnectTimeout !== null) return;

    const baseDelay = 1000;
    const delay = Math.min(baseDelay * Math.pow(2, this.reconnectAttempts), 30000);
    this.reconnectAttempts++;

    console.debug(`[Syncline] Scheduling reconnect in ${delay}ms (Attempt ${this.reconnectAttempts})`);
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
    this.ignoreChanges.clear();
    this.knownFiles.clear();
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

      if (this.client.is_connected()) {
        wasConnected = true;
        ticksDisconnected = 0;
        this.reconnectAttempts = 0;
        const count = this.client.doc_count();
        this.updateStatus("synced", `Syncline: ${count} file${count === 1 ? '' : 's'}`);
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

  // ---------------------------------------------------------------------------
  // Index management
  // ---------------------------------------------------------------------------

  onIndexUpdate() {
    if (!this.client) return;

    const keys = this.client.index_keys(); // these are UUIDs
    console.debug("[Syncline] Index update, keys:", keys);

    const reverseMap = this.reverseUuidMap();

    for (const uuid of keys) {
      if (!this.knownFiles.has(uuid)) {
        if (this.settings.knownFiles.includes(uuid)) {
          // UUID was known before but is no longer on the server → offline deletion
          console.debug("[Syncline] Detected offline deletion from index sync:", uuid);
          this.client.index_remove(uuid);
        } else {
          this.knownFiles.add(uuid);
          const path = reverseMap[uuid];
          const file = path ? this.app.vault.getAbstractFileByPath(path) : null;
          if (file instanceof TFile) {
            void this.addDocOnly(file, uuid);
          } else {
            void this.createFileFromRemote(uuid);
          }
        }
      }
    }

    // UUIDs that disappeared from the index → delete local file
    for (const uuid of Array.from(this.knownFiles)) {
      if (!keys.includes(uuid)) {
        this.knownFiles.delete(uuid);
        const path = reverseMap[uuid];
        if (path) {
          this.deleteLocalFile(path, uuid);
        }
      }
    }

    this.settings.knownFiles = Array.from(this.knownFiles);
    void this.saveSettings();
  }

  // ---------------------------------------------------------------------------
  // Document management
  // ---------------------------------------------------------------------------

  /** Subscribe to a doc by UUID. On first sync, merge local + remote via CRDT; thereafter handle remote updates. */
  async addDocOnly(file: TFile, uuid: string) {
    if (!this.client) return;

    let initialSyncDone = false;

    // Load persisted CRDT state and local file content in parallel
    const [persistedState, localContent] = await Promise.all([
      this.loadCrdtState(uuid),
      this.app.vault.read(file),
    ]);

    if (!this.client) return; // client may have been torn down during await

    const onSync = () => {
      // Ensure meta.path is set so the CLI client (and other WASM clients) know the file's path
      this.client?.set_meta_path(uuid, file.path);

      if (!initialSyncDone) {
        initialSyncDone = true;

        // After SyncStep2 merges remote edits into our doc (which already
        // contains our offline edits from step 2 below), write the fully
        // merged result to disk.
        const mergedContent = this.client?.get_text(uuid) || "";
        if (mergedContent !== localContent) {
          this.ignoreChanges.add(file.path);
          this.app.vault
            .modify(file, mergedContent)
            .finally(() => {
              setTimeout(() => this.ignoreChanges.delete(file.path), IGNORE_CHANGES_TIMEOUT_MS);
            })
            .catch((error) => {
              console.error(`[Syncline] Error updating ${file.path} after initial sync:`, error);
            });
        }

        // Persist the merged CRDT state
        void this.saveCrdtState(uuid);
      } else {
        // FIX(Issue #6): Suppress the file-watcher IMMEDIATELY when CRDT
        // is updated, BEFORE the async disk write in onRemoteUpdate().
        this.ignoreChanges.add(file.path);
        void this.onRemoteUpdate(uuid).then(() => this.saveCrdtState(uuid));
      }
    };

    if (persistedState) {
      // We have prior CRDT state — load it so the doc starts with full history.
      // add_doc_with_state applies the state before registering the observer,
      // so historical operations are not re-broadcast.
      this.client.add_doc_with_state(uuid, persistedState, onSync);

      // Apply offline edits IMMEDIATELY, before SyncStep2 arrives.
      // The diff is against the persisted CRDT content (not post-merge),
      // so it produces only the delta from offline file edits. When
      // SyncStep2 later merges remote operations, both sets of edits
      // coexist in the CRDT — no data loss.
      const persistedContent = this.client.get_text(uuid) || "";
      if (persistedContent !== localContent) {
        this.client.update(uuid, localContent);
      }
    } else {
      // First time syncing this doc (or state was lost). Use plain add_doc.
      // Push local content after sync completes (handled in onSync callback
      // only if the remote CRDT is empty — new file case).
      this.client.add_doc(uuid, onSync);

      // For a brand-new doc with no persisted state and no remote content,
      // push local content now. If remote has content (from SyncStep2),
      // the callback will write it to disk.
      const remoteContent = this.client.get_text(uuid) || "";
      if (remoteContent === "" && localContent !== "") {
        this.client.update(uuid, localContent);
      }
    }
  }

  /** Subscribe to a binary doc by UUID. Handles blob upload/download for non-text files. */
  addBinaryDocOnly(file: TFile, uuid: string) {
    if (!this.client) return;

    let initialSyncDone = false;

    // Metadata callback: fires when the CRDT meta document is updated
    const onMetaUpdate = () => {
      this.client?.set_meta_path(uuid, file.path);
      this.client?.set_meta_type(uuid, "binary");

      if (!initialSyncDone) {
        initialSyncDone = true;
        // On initial sync: compare local hash with remote hash
        this.app.vault
          .readBinary(file)
          .then(async (data) => {
            const localHash = await sha256Hex(data);
            const remoteHash = this.client?.get_blob_hash(uuid);

            if (!remoteHash || remoteHash === "") {
              // No remote hash yet — upload our data
              this.client?.set_blob_hash(uuid, localHash);
              this.client?.send_blob(uuid, new Uint8Array(data));
            } else if (remoteHash !== localHash) {
              // Remote has different content — request it
              this.client?.request_blob(uuid, remoteHash);
            }
            // else: hashes match, nothing to do
          })
          .catch((error) => {
            console.error(`[Syncline] Error reading binary file ${file.path}:`, error);
          });
      } else {
        // Subsequent meta updates: check if blob hash changed
        const remoteHash = this.client?.get_blob_hash(uuid);
        if (remoteHash && remoteHash !== "") {
          this.app.vault
            .readBinary(file)
            .then(async (data) => {
              const localHash = await sha256Hex(data);
              if (remoteHash !== localHash) {
                // Remote has newer blob — request it
                this.client?.request_blob(uuid, remoteHash);
              }
            })
            .catch((error) => {
              console.error(`[Syncline] Error reading binary file ${file.path}:`, error);
            });
        }
      }
    };

    // Blob callback: fires when MSG_BLOB_UPDATE arrives with binary data
    const onBlobReceived = (_docId: string, data: Uint8Array) => {
      const targetPath = this.client?.get_meta_path(uuid) ?? file.path;
      this.ignoreChanges.add(targetPath);

      const existingFile = this.app.vault.getAbstractFileByPath(targetPath);
      const promise: Promise<unknown> = existingFile instanceof TFile
        ? this.app.vault.modifyBinary(existingFile, data.buffer as ArrayBuffer)
        : this.ensureParentFolders(targetPath).then(() => {
            return this.app.vault.createBinary(targetPath, data.buffer as ArrayBuffer).then(() => {
              this.settings.uuidMap[targetPath] = uuid;
              void this.saveSettings();
            });
          });

      promise
        .then(() => {
          console.debug(`[Syncline] Wrote binary file ${targetPath} (${data.length} bytes)`);
        })
        .catch((error) => {
          console.error(`[Syncline] Failed to write binary file ${targetPath}:`, error);
        })
        .finally(() => {
          setTimeout(() => this.ignoreChanges.delete(targetPath), IGNORE_CHANGES_TIMEOUT_MS);
        });
    };

    this.client.add_binary_doc(uuid, onMetaUpdate, onBlobReceived);
  }

  /** Register a new file: assign UUID, track it, insert into index, subscribe. */
  addFile(file: TFile) {
    if (!this.client) return;

    const uuid = this.getOrCreateUuid(file.path);
    this.knownFiles.add(uuid);
    this.client.index_insert(uuid);
    if (isBinaryFile(file)) {
      this.addBinaryDocOnly(file, uuid);
    } else {
      this.addDocOnly(file, uuid);
    }
  }

  private async ensureParentFolders(filePath: string): Promise<void> {
    const parts = filePath.split("/");
    parts.pop();
    let currentPath = "";
    for (const part of parts) {
      currentPath = currentPath ? `${currentPath}/${part}` : part;
      const existing = this.app.vault.getAbstractFileByPath(currentPath);
      if (!existing) {
        try {
          await this.app.vault.createFolder(currentPath);
        } catch (e) {
          if (!this.app.vault.getAbstractFileByPath(currentPath)) {
            console.error(`[Syncline] Failed to create folder ${currentPath}:`, e);
            throw e;
          }
        }
      }
    }
  }

  /** Subscribe to a remote-only UUID and create the local file once meta.path is known. */
  createFileFromRemote(uuid: string) {
    if (!this.client) return;

    // We don't know yet if this is text or binary — use add_doc first,
    // and once meta.type arrives via CRDT we can decide.
    this.client.add_doc(uuid, () => {
      const path = this.client?.get_meta_path(uuid);
      const metaType = this.client?.get_meta_type(uuid);
      if (path) {
        // Record the path→uuid mapping now that we know it
        this.settings.uuidMap[path] = uuid;
        void this.saveSettings();

        if (metaType === "binary") {
          // Re-register as binary doc and request blob
          this.client?.remove_doc(uuid);
          const file = this.app.vault.getAbstractFileByPath(path);
          if (file instanceof TFile) {
            this.addBinaryDocOnly(file, uuid);
          } else {
            // Create placeholder, then register binary doc
            void this.ensureParentFolders(path).then(() => {
              void this.app.vault.createBinary(path, new ArrayBuffer(0)).then((newFile) => {
                this.addBinaryDocOnly(newFile, uuid);
              }).catch((error) => {
                console.error(`[Syncline] Failed to create binary file ${path}:`, error);
              });
            });
          }
        } else {
          void this.onRemoteUpdate(uuid);
        }
      }
    });
  }

  deleteLocalFile(filePath: string, uuid: string) {
    const file = this.app.vault.getAbstractFileByPath(filePath);
    if (file instanceof TFile) {
      this.ignoreChanges.add(filePath);
      this.app.fileManager.trashFile(file).catch((error) => {
        console.error(`[Syncline] Error deleting file ${filePath}:`, error);
      }).finally(() => {
        setTimeout(() => this.ignoreChanges.delete(filePath), IGNORE_CHANGES_TIMEOUT_MS);
      });
    }
    this.client?.remove_doc(uuid);
  }

  async onRemoteUpdate(uuid: string) {
    if (!this.client) return;

    const content = this.client.get_text(uuid);
    if (content == null) return;

    const metaPath = this.client.get_meta_path(uuid);
    const reverseMap = this.reverseUuidMap();
    const currentPath = reverseMap[uuid] ?? metaPath;

    if (!currentPath && !metaPath) {
      return; // Path not known yet
    }

    const targetPath = metaPath ?? currentPath;

    // Handle rename propagated from a remote client: meta.path changed
    if (metaPath && currentPath && metaPath !== currentPath) {
      const oldFile = this.app.vault.getAbstractFileByPath(currentPath);
      if (oldFile instanceof TFile) {
        this.ignoreChanges.add(currentPath);
        this.ignoreChanges.add(metaPath);
        try {
          await this.ensureParentFolders(metaPath);
          await this.app.fileManager.renameFile(oldFile, metaPath);
          delete this.settings.uuidMap[currentPath];
          this.settings.uuidMap[metaPath] = uuid;
          await this.saveSettings();
        } catch (error) {
          console.error(`[Syncline] Error renaming ${currentPath} → ${metaPath}:`, error);
        } finally {
          setTimeout(() => {
            this.ignoreChanges.delete(currentPath);
            this.ignoreChanges.delete(metaPath);
          }, IGNORE_CHANGES_TIMEOUT_MS);
        }
      }
    }

    const file = this.app.vault.getAbstractFileByPath(targetPath);

    if (file instanceof TFile) {
      try {
        const currentContent = await this.app.vault.read(file);
        if (currentContent === content) {
          // Disk already matches CRDT — clear ignore early, nothing to write.
          this.ignoreChanges.delete(targetPath);
          return;
        }
        // ignoreChanges is already set by the caller (addDocOnly callback)
        // but ensure it's set in case onRemoteUpdate is called from elsewhere.
        this.ignoreChanges.add(targetPath);
        await this.app.vault.modify(file, content);
      } catch (error) {
        console.error(`[Syncline] Error updating file ${targetPath}:`, error);
      } finally {
        // Clear the suppression after IGNORE_CHANGES_TIMEOUT_MS.
        // This MUST be longer than the 300 ms debounce on onFileModify so
        // that any watcher event caused by our vault.modify() call above
        // is still suppressed when the debounced handler fires.
        setTimeout(() => this.ignoreChanges.delete(targetPath), IGNORE_CHANGES_TIMEOUT_MS);
      }
    } else if (!file && targetPath) {
      try {
        await this.ensureParentFolders(targetPath);
        this.ignoreChanges.add(targetPath);
        await this.app.vault.create(targetPath, content);
        if (metaPath) {
          this.settings.uuidMap[metaPath] = uuid;
          await this.saveSettings();
        }
        this.knownFiles.add(uuid);
      } catch (error) {
        console.error(`[Syncline] Failed to create file ${targetPath} on remote update:`, error);
      } finally {
        setTimeout(() => this.ignoreChanges.delete(targetPath), IGNORE_CHANGES_TIMEOUT_MS);
      }
    }
  }

  // ---------------------------------------------------------------------------
  // Vault event handlers
  // ---------------------------------------------------------------------------

  onFileModify = debounce(async (file: TAbstractFile) => {
    if (!(file instanceof TFile)) return;
    if (this.ignoreChanges.has(file.path)) return;
    if (!this.client) return;

    const uuid = this.settings.uuidMap[file.path];
    if (!uuid) return;

    if (isBinaryFile(file)) {
      // Binary file: read bytes, compute hash, upload if changed
      try {
        const data = await this.app.vault.readBinary(file);
        if (this.ignoreChanges.has(file.path)) return;
        const hash = await sha256Hex(data);
        const currentHash = this.client.get_blob_hash(uuid);
        if (currentHash === hash) return; // unchanged
        this.client.set_blob_hash(uuid, hash);
        this.client.set_meta_type(uuid, "binary");
        this.client.send_blob(uuid, new Uint8Array(data));
      } catch (error) {
        console.error(`[Syncline] Error syncing binary file ${file.path}:`, error);
      }
    } else {
      // Text file: existing behavior
      try {
        const content = await this.app.vault.read(file);
        if (this.ignoreChanges.has(file.path)) return;
        const crdtContent = this.client.get_text(uuid);
        if (crdtContent === content) return;
        this.client.update(uuid, content);
        // Persist CRDT state so offline edits survive plugin restarts
        void this.saveCrdtState(uuid);
      } catch (error) {
        console.error("[Syncline] Error syncing:", error);
      }
    }
  }, 300);

  onFileCreate = (file: TAbstractFile) => {
    if (!(file instanceof TFile)) return;
    if (this.ignoreChanges.has(file.path)) return;

    this.addFile(file);
    this.settings.knownFiles = Array.from(this.knownFiles);
    void this.saveSettings();
  };

  onFileDelete = (file: TAbstractFile) => {
    if (!(file instanceof TFile)) return;
    if (this.ignoreChanges.has(file.path)) return;

    const uuid = this.settings.uuidMap[file.path];
    if (uuid) {
      this.client?.index_remove(uuid);
      this.client?.remove_doc(uuid);
      this.knownFiles.delete(uuid);
      delete this.settings.uuidMap[file.path];
    }

    this.settings.knownFiles = Array.from(this.knownFiles);
    void this.saveSettings();
  };

  onFileRename = (file: TAbstractFile, oldPath: string) => {
    if (this.ignoreChanges.has(file instanceof TFile ? file.path : oldPath)) return;

    const uuid = this.settings.uuidMap[oldPath];
    if (uuid) {
      // Keep the same UUID — only update the path mapping and broadcast via set_meta_path
      delete this.settings.uuidMap[oldPath];
      if (file instanceof TFile) {
        this.settings.uuidMap[file.path] = uuid;
        // This CRDT update broadcasts the new meta.path to all other clients
        this.client?.set_meta_path(uuid, file.path);
      } else {
        // Renamed to a non-file — remove from sync
        this.client?.index_remove(uuid);
        this.client?.remove_doc(uuid);
        this.knownFiles.delete(uuid);
      }
    } else if (file instanceof TFile) {
      // File was not previously tracked
      this.addFile(file);
    }

    this.settings.knownFiles = Array.from(this.knownFiles);
    void this.saveSettings();
  };
}

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
        toggle
          .setValue(this.plugin.settings.autoSync)
          .onChange(async (value) => {
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
  }
}
