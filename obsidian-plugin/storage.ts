// Storage abstraction for Syncline's CRDT state.
//
// The plugin used to write its state into the vault's hidden config
// folder (`${configDir}/plugins/syncline/v1/manifest.bin` etc.). That
// made the state visible to the hidden-file scanner introduced in
// PR #89, requiring a self-exclusion ignore rule and a hash-equality
// guard to prevent the plugin from re-uploading its own state in a
// loop. Both were band-aids: the state remained physically observable.
//
// This module relocates the bulk of the state into IndexedDB, where
// the watcher can't see it at all. LiveSync uses the same approach
// (via PouchDB → IndexedDB). Cross-platform: works on desktop, iOS,
// and Android.
//
// Layout:
//
//   IndexedDB ("syncline-<vaultId>")
//     ├── store "manifest" — key "current"  → Uint8Array
//     └── store "content"  — key <nodeId>   → Uint8Array
//
//   localStorage (per-vault via app.saveLocalStorage)
//     ├── "syncline:vault-id"  → string (UUID, minted on first run)
//     └── "syncline:lamport"   → number
//
//   ${configDir}/plugins/syncline/data.json
//     → plugin settings (server URL, actorId, configSync flags)
//
// Settings stay on disk so they remain inspectable by the user and
// reachable by Obsidian's standard `Plugin.loadData` / `saveData`.
// Everything else is invisible to the vault adapter.

import type { App } from "obsidian";

const SCHEMA_VERSION = 1;
const MANIFEST_STORE = "manifest";
const CONTENT_STORE = "content";
const MANIFEST_KEY = "current";

export const VAULT_ID_LS_KEY = "syncline:vault-id";
export const LAMPORT_LS_KEY = "syncline:lamport";

/**
 * Stable per-vault id used to namespace the IndexedDB database name.
 *
 * `app.loadLocalStorage` / `saveLocalStorage` are themselves scoped
 * per-vault by Obsidian, so a UUID stored under those keys is
 * naturally isolated. Generated on first access if absent.
 *
 * Why not `app.appId`? It's an undocumented private field — present
 * today but no API guarantee. A self-managed UUID is bulletproof.
 */
export function getOrMintVaultId(app: App): string {
  const existing: unknown = app.loadLocalStorage(VAULT_ID_LS_KEY);
  if (typeof existing === "string" && existing.length > 0) return existing;
  const fresh =
    typeof crypto !== "undefined" && typeof crypto.randomUUID === "function"
      ? crypto.randomUUID()
      : `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
  app.saveLocalStorage(VAULT_ID_LS_KEY, fresh);
  return fresh;
}

/**
 * Storage interface — narrow surface so callers don't depend on the
 * IndexedDB specifics. Manifest and lamport are conceptually a pair
 * (lamport tracks the most recent op in the manifest), but are stored
 * separately because IndexedDB is the right place for opaque bytes
 * and localStorage is the right place for tiny scalars. A small
 * inconsistency window between the two writes is tolerable: lamport
 * only gates an optimization (skipping a Step1 sync) and the manifest
 * itself carries enough state to rebuild from.
 */
export interface SynclineStorage {
  /** Most recent persisted manifest snapshot, or null if none yet. */
  getManifest(): Promise<Uint8Array | null>;
  /** Replace the manifest snapshot. */
  setManifest(bytes: Uint8Array): Promise<void>;

  /** Last persisted lamport counter, or 0 if none. */
  getLamport(): number;
  /** Replace the lamport counter. */
  setLamport(n: number): void;

  /** Per-nodeId content subdoc snapshot, or null if absent. */
  getContent(nodeId: string): Promise<Uint8Array | null>;
  /** Replace the content snapshot for a node. */
  putContent(nodeId: string, bytes: Uint8Array): Promise<void>;
  /** Remove a content snapshot. Idempotent. */
  deleteContent(nodeId: string): Promise<void>;

  /** Used by the migration code to wipe everything for a clean restart. */
  reset(): Promise<void>;
}

/**
 * IndexedDB-backed `SynclineStorage`. Manifest and content blobs go
 * into IndexedDB; lamport counter goes into localStorage via the App
 * helpers. Cross-platform — works on desktop Electron, iOS, and
 * Android, all of which expose IndexedDB through the standard
 * `globalThis.indexedDB`.
 */
export class IndexedDBSynclineStorage implements SynclineStorage {
  private readonly dbName: string;
  private readonly app: App;
  private dbPromise: Promise<IDBDatabase> | null = null;

  constructor(app: App, vaultId: string) {
    this.app = app;
    // Suffix with vault id so multiple vaults on the same machine
    // (sharing a single Obsidian process on mobile, or separate
    // BrowserWindows on desktop) don't collide.
    this.dbName = `syncline-${vaultId}`;
  }

  /** Lazily open the database. Repeated callers share the connection. */
  private openDB(): Promise<IDBDatabase> {
    if (this.dbPromise) return this.dbPromise;
    this.dbPromise = new Promise<IDBDatabase>((resolve, reject) => {
      const req = indexedDB.open(this.dbName, SCHEMA_VERSION);
      req.onupgradeneeded = () => {
        const db = req.result;
        if (!db.objectStoreNames.contains(MANIFEST_STORE)) {
          db.createObjectStore(MANIFEST_STORE);
        }
        if (!db.objectStoreNames.contains(CONTENT_STORE)) {
          db.createObjectStore(CONTENT_STORE);
        }
      };
      req.onsuccess = () => {
        const db = req.result;
        // Drop the cached promise on close so a subsequent call
        // re-opens cleanly. Fires on browser tab close, IndexedDB
        // version-change events, etc.
        db.onclose = () => {
          this.dbPromise = null;
        };
        db.onversionchange = () => {
          db.close();
          this.dbPromise = null;
        };
        resolve(db);
      };
      req.onerror = () => reject(req.error ?? new Error("indexedDB request failed"));
      req.onblocked = () =>
        reject(new Error(`IndexedDB upgrade blocked for ${this.dbName}`));
    });
    return this.dbPromise;
  }

  private async getValue<T>(
    store: string,
    key: string,
  ): Promise<T | null> {
    const db = await this.openDB();
    return new Promise<T | null>((resolve, reject) => {
      const tx = db.transaction(store, "readonly");
      const req = tx.objectStore(store).get(key);
      req.onsuccess = () => resolve((req.result as T | undefined) ?? null);
      req.onerror = () => reject(req.error ?? new Error("indexedDB request failed"));
    });
  }

  private async putValue(
    store: string,
    key: string,
    value: unknown,
  ): Promise<void> {
    const db = await this.openDB();
    return new Promise<void>((resolve, reject) => {
      const tx = db.transaction(store, "readwrite");
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error ?? new Error("indexedDB transaction failed"));
      tx.onabort = () =>
        reject(tx.error || new Error("transaction aborted"));
      tx.objectStore(store).put(value, key);
    });
  }

  private async deleteValue(store: string, key: string): Promise<void> {
    const db = await this.openDB();
    return new Promise<void>((resolve, reject) => {
      const tx = db.transaction(store, "readwrite");
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error ?? new Error("indexedDB transaction failed"));
      tx.onabort = () =>
        reject(tx.error || new Error("transaction aborted"));
      tx.objectStore(store).delete(key);
    });
  }

  async getManifest(): Promise<Uint8Array | null> {
    return this.getValue<Uint8Array>(MANIFEST_STORE, MANIFEST_KEY);
  }

  async setManifest(bytes: Uint8Array): Promise<void> {
    return this.putValue(MANIFEST_STORE, MANIFEST_KEY, bytes);
  }

  getLamport(): number {
    const raw: unknown = this.app.loadLocalStorage(LAMPORT_LS_KEY);
    if (typeof raw === "number" && Number.isFinite(raw)) return raw;
    if (typeof raw === "string") {
      const n = Number.parseInt(raw, 10);
      return Number.isFinite(n) ? n : 0;
    }
    return 0;
  }

  setLamport(n: number): void {
    this.app.saveLocalStorage(LAMPORT_LS_KEY, n);
  }

  async getContent(nodeId: string): Promise<Uint8Array | null> {
    return this.getValue<Uint8Array>(CONTENT_STORE, nodeId);
  }

  async putContent(nodeId: string, bytes: Uint8Array): Promise<void> {
    return this.putValue(CONTENT_STORE, nodeId, bytes);
  }

  async deleteContent(nodeId: string): Promise<void> {
    return this.deleteValue(CONTENT_STORE, nodeId);
  }

  async reset(): Promise<void> {
    const db = await this.openDB();
    await new Promise<void>((resolve, reject) => {
      const tx = db.transaction([MANIFEST_STORE, CONTENT_STORE], "readwrite");
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error ?? new Error("indexedDB transaction failed"));
      tx.onabort = () =>
        reject(tx.error || new Error("transaction aborted"));
      tx.objectStore(MANIFEST_STORE).clear();
      tx.objectStore(CONTENT_STORE).clear();
    });
    this.app.saveLocalStorage(LAMPORT_LS_KEY, null);
  }
}
