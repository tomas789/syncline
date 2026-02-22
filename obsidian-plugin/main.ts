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
}

const DEFAULT_SETTINGS: SynclineSettings = {
  serverUrl: "ws://localhost:3030/sync",
  autoSync: true,
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
  remove_doc(doc_id: string): void;
  get_text(doc_id: string): string | undefined;
  update(doc_id: string, content: string): void;
  set_text(doc_id: string, content: string): void;
  is_connected(): boolean;
  doc_count(): number;
  create_index(callback?: () => void): void;
  index_insert(key: string): void;
  index_remove(key: string): void;
  index_keys(): string[];
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
      text: "Syncline: Disconnected",
    });

    this.statusBarItem.onClickEvent(() => {
      this.showStatusDetails();
    });

    this.ribbonIconEl = this.addRibbonIcon("sync", "Syncline: Disconnected", () => {
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
  }

  async saveSettings() {
    await this.saveData(this.settings);
  }

  updateStatus(status: SyncStatus, text?: string) {
    this.syncStatus = status;
    this.statusIcon.className = `status-icon ${status}`;

    const statusTexts: Record<SyncStatus, string> = {
      synced: "Syncline: Synced",
      syncing: "Syncline: Syncing...",
      error: "Syncline: Error",
      disconnected: "Syncline: Disconnected",
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

  async connect() {
    if (this.reconnectTimeout !== null) {
      window.clearTimeout(this.reconnectTimeout);
      this.reconnectTimeout = null;
    }

    try {
      this.updateStatus("syncing", "Syncline: Connecting...");
      console.debug("[Syncline] Connecting to:", this.settings.serverUrl);

      const wasmMod = await initWasm();
      this.client = new wasmMod.SynclineClient(this.settings.serverUrl);

      // Create index document first
      this.client.create_index(() => {
        this.onIndexUpdate();
      });

      // Add all files to index BEFORE connecting
      const files = this.app.vault.getMarkdownFiles();
      console.debug("[Syncline] Adding", files.length, "files to index");

      for (const file of files) {
        const docId = file.path;
        this.client.index_insert(docId);
        this.knownFiles.add(docId);
      }

      // Now connect - will send index with all files
      this.client.connect();

      // Add docs for syncing content
      for (const file of files) {
        this.addDocOnly(file);
      }

      this.startStatusCheck();
    } catch (error) {
      console.error("[Syncline] Connection error:", error);
      this.updateStatus("error");
      this.scheduleReconnect();
    }
  }

  scheduleReconnect() {
    if (this.reconnectTimeout !== null) return;
    
    // Initial retry is fast (e.g., 1000ms), then exponential backoff, max 30000ms
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
    this.client = null;
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
        this.reconnectAttempts = 0; // reset attempts on successful connection
        const count = this.client.doc_count();
        this.updateStatus("synced", `Syncline: ${count} files`);
      } else {
        if (wasConnected) {
          // Connection dropped after having been connected
          this.updateStatus("error", "Syncline: Connection lost");
          wasConnected = false;
          ticksDisconnected = 0;
          this.scheduleReconnect();
        } else {
          // Currently trying to connect, or server is down
          ticksDisconnected++;
          if (ticksDisconnected > 5 && this.reconnectTimeout === null) {
            // Failed to connect after 5 seconds
            console.warn("[Syncline] Connection timed out after 5s");
            this.updateStatus("error", "Syncline: Connection failed");
            this.scheduleReconnect();
          } else if (this.syncStatus !== "syncing" && this.syncStatus !== "error") {
            this.updateStatus("syncing", "Syncline: Connecting...");
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

  onIndexUpdate() {
    if (!this.client) return;

    const keys = this.client.index_keys();
    console.debug("[Syncline] Index update, keys:", keys);

    for (const key of keys) {
      if (!this.knownFiles.has(key)) {
        this.knownFiles.add(key);
        const file = this.app.vault.getAbstractFileByPath(key);
        if (file instanceof TFile) {
          this.addDocOnly(file);
        } else {
          void this.createFileFromRemote(key);
        }
      }
    }

    for (const known of Array.from(this.knownFiles)) {
      if (!keys.includes(known)) {
        this.knownFiles.delete(known);
        this.deleteLocalFile(known);
      }
    }
  }

  addDocOnly(file: TFile) {
    if (!this.client) return;
    
    const docId = file.path;
    let initialSyncDone = false;
    
    this.client.add_doc(docId, () => {
      if (!initialSyncDone) {
        initialSyncDone = true;
        this.app.vault.read(file).then(content => {
          const remoteContent = this.client?.get_text(docId) || "";

          // If local file is empty (e.g. from iCloud stub or newly created), 
          // but remote has content, favor the remote content instead of deleting it.
          if (content === "" && remoteContent !== "") {
            this.ignoreChanges.add(docId);
            this.app.vault.modify(file, remoteContent).finally(() => {
              setTimeout(() => this.ignoreChanges.delete(docId), 100);
            }).catch(error => {
              console.error(`[Syncline] Error updating file ${docId} after initial merge:`, error);
            });
            return;
          }

          this.client?.update(docId, content);
          
          const merged = this.client?.get_text(docId);
          if (merged && merged !== content) {
            this.ignoreChanges.add(docId);
            this.app.vault.modify(file, merged).finally(() => {
              setTimeout(() => this.ignoreChanges.delete(docId), 100);
            }).catch(error => {
              console.error(`[Syncline] Error updating file ${docId} after initial merge:`, error);
            });
          }
        }).catch(error => {
          console.error(`[Syncline] Error reading file ${docId}:`, error);
        });
      } else {
        void this.onRemoteUpdate(docId);
      }
    });
  }

  addFile(file: TFile) {
    if (!this.client) return;

    const docId = file.path;
    this.client.index_insert(docId);
    this.knownFiles.add(docId);
    this.addDocOnly(file);
  }

  private async ensureParentFolders(filePath: string): Promise<void> {
    const parts = filePath.split("/");
    parts.pop(); // remove filename, keep only directory parts
    let currentPath = "";
    for (const part of parts) {
      currentPath = currentPath ? `${currentPath}/${part}` : part;
      const existing = this.app.vault.getAbstractFileByPath(currentPath);
      if (!existing) {
        try {
          await this.app.vault.createFolder(currentPath);
        } catch (e) {
          // createFolder throws if folder already exists (race condition); re-check
          if (!this.app.vault.getAbstractFileByPath(currentPath)) {
            console.error(
              `[Syncline] Failed to create folder ${currentPath}:`,
              e,
            );
            throw e;
          }
        }
      }
    }
  }

  async createFileFromRemote(docId: string) {
    if (!this.client) return;

    this.client.add_doc(docId, () => {
      this.onRemoteUpdate(docId);
    });

    await new Promise((resolve) => setTimeout(resolve, 500));

    const content = this.client.get_text(docId);
    if (content) {
      const file = this.app.vault.getAbstractFileByPath(docId);
      if (!(file instanceof TFile)) {
        try {
          await this.ensureParentFolders(docId);
          await this.app.vault.create(docId, content);
          this.knownFiles.add(docId);
        } catch (error) {
          console.error(`[Syncline] Failed to create file ${docId}:`, error);
        }
      }
    }
  }

  deleteLocalFile(path: string) {
    const file = this.app.vault.getAbstractFileByPath(path);
    if (file instanceof TFile) {
      this.app.fileManager.trashFile(file).catch((error) => {
        console.error(`[Syncline] Error deleting file ${path}:`, error);
      });
    }
    this.client?.remove_doc(path);
  }

  async onRemoteUpdate(docId: string) {
    if (!this.client) return;

    const content = this.client.get_text(docId);
    if (content == null) return;

    const file = this.app.vault.getAbstractFileByPath(docId);

    if (file instanceof TFile) {
      try {
        const currentContent = await this.app.vault.read(file);
        if (currentContent === content) {
          return;
        }

        this.ignoreChanges.add(docId);
        await this.app.vault.modify(file, content);
      } catch (error) {
        console.error(`[Syncline] Error updating file ${docId}:`, error);
      } finally {
        setTimeout(() => this.ignoreChanges.delete(docId), 100);
      }
    }
  }

  onFileModify = debounce(async (file: TAbstractFile) => {
    if (!(file instanceof TFile)) return;
    if (!["md", "txt"].includes(file.extension ?? "")) return;
    if (this.ignoreChanges.has(file.path)) return;
    if (!this.client) return;

    try {
      const content = await this.app.vault.read(file);
      this.client.update(file.path, content);
    } catch (error) {
      console.error("[Syncline] Error syncing:", error);
    }
  }, 300);

  onFileCreate = (file: TAbstractFile) => {
    if (!(file instanceof TFile)) return;
    if (!["md", "txt"].includes(file.extension ?? "")) return;
    this.addFile(file);
  };

  onFileDelete = (file: TAbstractFile) => {
    if (!(file instanceof TFile)) return;
    const docId = file.path;

    this.client?.index_remove(docId);
    this.client?.remove_doc(docId);
    this.knownFiles.delete(docId);
  };

  onFileRename = (file: TAbstractFile, oldPath: string) => {
    this.client?.index_remove(oldPath);
    this.client?.remove_doc(oldPath);
    this.knownFiles.delete(oldPath);

    if (file instanceof TFile && ["md", "txt"].includes(file.extension ?? "")) {
      this.addFile(file);
    }
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
