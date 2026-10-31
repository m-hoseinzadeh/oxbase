/**
 * Typed wrappers around the Tauri commands exposed by `crates/app-tauri`.
 *
 * Control calls return JSON; result pages come back as raw Arrow IPC stream bytes
 * (`tauri::ipc::Response`) and are decoded here with `apache-arrow`, never as JSON.
 */
import { invoke, isTauri } from "@tauri-apps/api/core";
import { type Table, tableFromIPC } from "apache-arrow";

/** Mirrors `app_tauri::AppInfo` (serialised camelCase). */
export interface AppInfo {
  version: string;
  platform: string;
  arch: string;
}

/** Thrown when the UI runs in a plain browser (e.g. `pnpm dev`) without the Rust core. */
export class NoCoreError extends Error {
  constructor() {
    super("Not running inside the oxbase desktop shell");
    this.name = "NoCoreError";
  }
}

function requireCore(): void {
  if (!isTauri()) {
    throw new NoCoreError();
  }
}

export async function getAppInfo(): Promise<AppInfo> {
  requireCore();
  return invoke<AppInfo>("app_info");
}

/** Raw-bytes command results can surface as an ArrayBuffer, a typed array or a number array. */
export type RawBytes = ArrayBuffer | Uint8Array | number[];

export function toBytes(raw: RawBytes): Uint8Array {
  if (raw instanceof Uint8Array) return raw;
  if (raw instanceof ArrayBuffer) return new Uint8Array(raw);
  return Uint8Array.from(raw);
}

/**
 * Decodes one Arrow IPC stream (schema + record batches) into a table.
 * `apache-arrow` returns an empty table for bytes it cannot parse, so a page without a
 * schema is rejected here instead of being shown as "0 rows".
 */
export function decodeArrowPage(raw: RawBytes): Table {
  const table = tableFromIPC(toBytes(raw));
  if (table.schema.fields.length === 0) {
    throw new Error("Invalid Arrow IPC page: no schema");
  }
  return table;
}

export async function fetchDemoPage(): Promise<Table> {
  requireCore();
  const raw = await invoke<RawBytes>("demo_arrow_page");
  return decodeArrowPage(raw);
}
