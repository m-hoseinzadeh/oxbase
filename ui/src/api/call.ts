/** Shared command invoker of the workbench wrappers: failures become `{ category, message }`. */
import { invoke, isTauri } from "@tauri-apps/api/core";
import { toCommandError } from "../connections/api";
import { NoCoreError } from "../ipc";

export async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauri()) throw toCommandError(new NoCoreError());
  try {
    return await invoke<T>(cmd, args);
  } catch (error) {
    throw toCommandError(error);
  }
}
