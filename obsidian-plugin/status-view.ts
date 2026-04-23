// Syncline status view.
//
// Rendered as a sidebar ItemView so it works identically on desktop and
// Obsidian Mobile (status bar items don't render on iOS). Everything the
// user or a developer wants to see about the current sync state lives
// here: the at-a-glance status line, recent activity, and — when debug
// mode is on in settings — a live dump of the CRDT-side bookkeeping
// plus a way to copy diagnostics to the clipboard.

import { ButtonComponent, ItemView, Notice, WorkspaceLeaf } from "obsidian";
import type SynclinePlugin from "./main";
import type { SyncEvent } from "./main";

export const SYNCLINE_VIEW_TYPE = "syncline-status";

/** How long (ms) to show "verify pending…" before calling it converged. */
const VERIFY_WINDOW_MS = 3000;

/** Normal-mode event count. Debug mode reveals the full ring. */
const NORMAL_EVENT_ROWS = 10;

export class SynclineStatusView extends ItemView {
  private plugin: SynclinePlugin;
  private refreshTimer: number | null = null;
  private verifyDeadline: number | null = null;

  constructor(leaf: WorkspaceLeaf, plugin: SynclinePlugin) {
    super(leaf);
    this.plugin = plugin;
  }

  getViewType(): string {
    return SYNCLINE_VIEW_TYPE;
  }

  getDisplayText(): string {
    return "Syncline";
  }

  getIcon(): string {
    return "sync";
  }

  async onOpen(): Promise<void> {
    this.render();
    // Re-render every second. `render()` is cheap (no allocation churn
    // relative to Obsidian's file tree refresh), and we want the
    // "N seconds ago" labels in the activity log to move forward without
    // the user having to click.
    this.refreshTimer = window.setInterval(() => this.render(), 1000);
  }

  async onClose(): Promise<void> {
    if (this.refreshTimer !== null) {
      window.clearInterval(this.refreshTimer);
      this.refreshTimer = null;
    }
  }

  private render(): void {
    const p = this.plugin;
    const root = this.containerEl.children[1] as HTMLElement;
    root.empty();
    root.addClass("syncline-view");

    // ---- Header: big colored indicator + one-line status ----
    const header = root.createDiv({ cls: "syncline-view-header" });
    header.createDiv({ cls: `syncline-view-dot ${p.syncStatus}` });
    const hb = header.createDiv({ cls: "syncline-view-header-body" });
    hb.createDiv({ cls: "syncline-view-title", text: p.statusTitle() });
    hb.createDiv({ cls: "syncline-view-subtitle", text: p.statusSubtitle() });

    // ---- Counters row ----
    const counters = root.createDiv({ cls: "syncline-view-counters" });
    this.counter(counters, String(p.lastProjection.size), "files");
    const pending = p.pendingCount();
    if (pending > 0) {
      this.counter(counters, String(pending), "pending");
    }
    const last = p.syncEvents.lastEventTime();
    this.counter(
      counters,
      last === null ? "—" : relTime(last),
      "last activity"
    );

    // ---- Action buttons ----
    const actions = root.createDiv({ cls: "syncline-view-actions" });

    new ButtonComponent(actions)
      .setButtonText("Reconnect")
      .setIcon("refresh-cw")
      .onClick(async () => {
        p.disconnect();
        await p.connect();
      });

    new ButtonComponent(actions)
      .setButtonText("Verify")
      .setIcon("check-circle")
      .onClick(() => {
        if (!p.client) {
          new Notice("Not connected — nothing to verify");
          return;
        }
        try {
          p.client.sendVerify();
          p.syncEvents.log("info", "Sent MSG_MANIFEST_VERIFY");
          this.verifyDeadline = Date.now() + VERIFY_WINDOW_MS;
          new Notice(
            "Verify sent; converged if no remote sync arrives in " +
              `${VERIFY_WINDOW_MS / 1000}s`
          );
        } catch (e) {
          new Notice(`Verify failed: ${String((e as Error)?.message ?? e)}`);
        }
      });

    if (p.settings.debugMode) {
      new ButtonComponent(actions)
        .setButtonText("Copy diagnostics")
        .setIcon("clipboard-copy")
        .onClick(() => p.copyDiagnostics());
    }

    // ---- Verify result banner (transient) ----
    if (this.verifyDeadline !== null) {
      if (Date.now() >= this.verifyDeadline) {
        this.verifyDeadline = null;
      } else {
        const banner = root.createDiv({ cls: "syncline-view-verify" });
        banner.setText(
          `Verify pending — ${Math.ceil(
            (this.verifyDeadline - Date.now()) / 1000
          )}s to go`
        );
      }
    }

    // ---- Recent activity log ----
    const log = root.createDiv({ cls: "syncline-view-log" });
    log.createEl("h4", { text: "Recent activity" });
    const events = p.settings.debugMode
      ? p.syncEvents.recent(500)
      : p.syncEvents.recent(NORMAL_EVENT_ROWS);
    if (events.length === 0) {
      log.createDiv({ cls: "syncline-view-log-empty", text: "No activity yet." });
    } else {
      for (const ev of events) {
        const row = log.createDiv({
          cls: `syncline-view-log-row syncline-kind-${ev.kind}`,
        });
        row.createSpan({ cls: "syncline-view-log-time", text: relTime(ev.t) });
        row.createSpan({
          cls: `syncline-view-log-kind kind-${ev.kind}`,
          text: ev.kind,
        });
        row.createSpan({ cls: "syncline-view-log-msg", text: ev.message });
      }
    }

    // ---- Debug-only details ----
    if (p.settings.debugMode) {
      const debug = root.createDiv({ cls: "syncline-view-debug" });
      debug.createEl("h4", { text: "Debug" });
      const dl = debug.createEl("dl");
      const add = (k: string, v: string) => {
        dl.createEl("dt", { text: k });
        dl.createEl("dd", { text: v });
      };
      add("Server URL", p.settings.serverUrl);
      add("Actor ID", p.client?.actorId() ?? "—");
      add("Lamport", String(p.client?.lamport() ?? "—"));
      add("Projection size", String(p.lastProjection.size));
      add("Subscribed subdocs", String(p.subscribedContent.size));
      add("Requested blobs", String(p.requestedBlobs.size));
      add("Reconnect attempts", String(p.reconnectAttempts));
      add(
        "WS state",
        p.client?.isConnected() === true ? "open" : "closed / pending",
      );
    }
  }

  private counter(parent: HTMLElement, value: string, label: string): void {
    const c = parent.createDiv({ cls: "syncline-view-counter" });
    c.createDiv({ cls: "syncline-view-counter-val", text: value });
    c.createDiv({ cls: "syncline-view-counter-label", text: label });
  }
}

// Render `ms-ago` labels in a compact, always-the-same width-ish form.
function relTime(t: number): string {
  const ms = Date.now() - t;
  if (ms < 0) return "now";
  if (ms < 1_000) return "just now";
  if (ms < 60_000) return `${Math.floor(ms / 1_000)}s ago`;
  if (ms < 3_600_000) return `${Math.floor(ms / 60_000)}m ago`;
  return `${Math.floor(ms / 3_600_000)}h ago`;
}

// Re-export helper for main.ts so the plugin can use it in status line
// text without two copies.
export { relTime };

// Re-exported SyncEvent so the view's type imports stay symmetric with
// main.ts (which owns the emitter).
export type { SyncEvent };
