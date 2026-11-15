/**
 * App auto-update.
 *
 * Wraps `@tauri-apps/plugin-updater`: the Rust side signs and serves
 * `latest.json` from the GitHub release assets (see .github/workflows/release.yml),
 * this just checks it, reports what it finds, and — on request — downloads,
 * installs and relaunches. A browser dev session has no Tauri runtime, so
 * every action here is a no-op outside of it.
 */

import { create } from "zustand";
import type { Update } from "@tauri-apps/plugin-updater";

import { isTauri } from "../ipc/api";

export type UpdateStatus =
  | "idle"
  | "checking"
  | "upToDate"
  | "available"
  | "installing"
  | "error";

interface UpdateState {
  status: UpdateStatus;
  version: string | null;
  body: string | null;
  progress: number | null;
  error: string | null;
  check: () => Promise<void>;
  install: () => Promise<void>;
}

let pendingUpdate: Update | null = null;

export const useUpdateStore = create<UpdateState>((set, get) => ({
  status: "idle",
  version: null,
  body: null,
  progress: null,
  error: null,

  check: async () => {
    if (!isTauri()) return;
    const { status } = get();
    if (status === "checking" || status === "installing") return;

    set({ status: "checking", error: null });
    try {
      const { check: checkForUpdate } = await import("@tauri-apps/plugin-updater");
      const update = await checkForUpdate();
      pendingUpdate = update;
      if (update) {
        set({ status: "available", version: update.version, body: update.body ?? null });
      } else {
        set({ status: "upToDate", version: null, body: null });
      }
    } catch (error) {
      set({ status: "error", error: error instanceof Error ? error.message : String(error) });
    }
  },

  install: async () => {
    const update = pendingUpdate;
    if (!update) return;

    set({ status: "installing", progress: 0, error: null });
    try {
      let total = 0;
      let downloaded = 0;
      await update.downloadAndInstall((event) => {
        if (event.event === "Started") {
          total = event.data.contentLength ?? 0;
        } else if (event.event === "Progress") {
          downloaded += event.data.chunkLength;
          set({ progress: total > 0 ? Math.min(99, Math.round((downloaded / total) * 100)) : null });
        } else if (event.event === "Finished") {
          set({ progress: 100 });
        }
      });

      const { relaunch } = await import("@tauri-apps/plugin-process");
      await relaunch();
    } catch (error) {
      set({ status: "error", error: error instanceof Error ? error.message : String(error) });
    }
  },
}));
