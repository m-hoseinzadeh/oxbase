/**
 * Typed wrappers for the connection commands of `crates/app-tauri` and the native file picker.
 * Every failure is normalised to a {@link CommandError} (`{ category, message }`).
 */
import { invoke, isTauri } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { NoCoreError } from "../ipc";
import type {
  CommandError,
  ConnectionInfo,
  ConnectionProfile,
  ErrorCategory,
  SecretInput,
  SessionSummary,
} from "./types";

const CATEGORIES: readonly ErrorCategory[] = [
  "connection",
  "auth",
  "timeout",
  "cancelled",
  "unsupported",
  "sql",
  "not_found",
  "invalid_input",
  "io",
  "internal",
];

function isCommandError(value: unknown): value is CommandError {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  return (
    typeof v.message === "string" &&
    typeof v.category === "string" &&
    (CATEGORIES as readonly string[]).includes(v.category)
  );
}

/** Turns anything a command rejected with into a `{ category, message }` error. */
export function toCommandError(error: unknown): CommandError {
  if (isCommandError(error)) return { category: error.category, message: error.message };
  if (error instanceof NoCoreError) {
    return { category: "unsupported", message: "Connections need the oxbase desktop app." };
  }
  const message = error instanceof Error ? error.message : String(error);
  return { category: "internal", message: message || "Unknown error" };
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauri()) throw toCommandError(new NoCoreError());
  try {
    return await invoke<T>(cmd, args);
  } catch (error) {
    throw toCommandError(error);
  }
}

/** Drops empty values so nothing blank is sent as a secret. */
function compactSecrets(secrets: SecretInput): SecretInput {
  const out: SecretInput = {};
  if (secrets.password) out.password = secrets.password;
  if (secrets.ssh_password) out.ssh_password = secrets.ssh_password;
  if (secrets.ssh_key_passphrase) out.ssh_key_passphrase = secrets.ssh_key_passphrase;
  return out;
}

export function listProfiles(): Promise<ConnectionProfile[]> {
  return call("list_profiles");
}

export function saveProfile(
  profile: ConnectionProfile,
  secrets: SecretInput,
): Promise<ConnectionProfile> {
  return call("save_profile", { profile, secrets: compactSecrets(secrets) });
}

export function deleteProfile(id: string): Promise<boolean> {
  return call("delete_profile", { id });
}

export function testConnection(
  profile: ConnectionProfile,
  secrets: SecretInput,
  requestId?: string,
): Promise<ConnectionInfo> {
  return call("test_connection", {
    profile,
    secrets: compactSecrets(secrets),
    requestId: requestId ?? null,
  });
}

export function openConnection(
  id: string,
  secrets: SecretInput,
  requestId?: string,
): Promise<SessionSummary> {
  return call("open_connection", {
    id,
    secrets: compactSecrets(secrets),
    requestId: requestId ?? null,
  });
}

export function cancelRequest(requestId: string): Promise<boolean> {
  return call("cancel_request", { requestId });
}

export function closeConnection(sessionId: string): Promise<void> {
  return call("close_connection", { sessionId });
}

export function listOpenConnections(): Promise<SessionSummary[]> {
  return call("list_open_connections");
}

let requestCounter = 0;

/** A fresh id for a cancellable request. */
export function newRequestId(): string {
  requestCounter += 1;
  return `req-${Date.now().toString(36)}-${requestCounter}`;
}

export interface FileFilter {
  name: string;
  extensions: string[];
}

/** Native "open file" picker. Resolves to the chosen path, or `null` when cancelled. */
export async function pickFile(title: string, filters?: FileFilter[]): Promise<string | null> {
  if (!isTauri()) return null;
  const picked = await open({
    title,
    multiple: false,
    directory: false,
    ...(filters ? { filters } : {}),
  });
  return typeof picked === "string" ? picked : null;
}
