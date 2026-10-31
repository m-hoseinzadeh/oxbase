import { vi } from "vitest";

/** Stand-in for `@tauri-apps/api/core`'s `Channel`: tests call `onmessage` themselves. */
class Channel<T> {
  onmessage: (message: T) => void = () => {};
}

/** Shared mock of `@tauri-apps/api/core`; tests set `invoke`'s behaviour per case. */
export const tauriCore = {
  Channel,
  invoke: vi.fn<(cmd: string, args?: unknown) => Promise<unknown>>(),
  isTauri: vi.fn<() => boolean>(() => true),
};
