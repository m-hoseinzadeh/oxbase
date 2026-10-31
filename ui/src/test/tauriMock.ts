import { vi } from "vitest";

/** Shared mock of `@tauri-apps/api/core`; tests set `invoke`'s behaviour per case. */
export const tauriCore = {
  invoke: vi.fn<(cmd: string, args?: unknown) => Promise<unknown>>(),
  isTauri: vi.fn<() => boolean>(() => true),
};
