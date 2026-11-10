/**
 * M7 MongoDB and Redis: payload types and typed wrappers for the commands of
 * `crates/app-tauri` (src/nosql.rs), docs/m7-contracts.md §8.
 *
 * - Documents: `documentFind` / `documentAggregate` start ordinary query runs (events like
 *   `queryRun`, pages with `queryPage`, the hidden `$document` column holds each row's whole
 *   document as canonical Extended JSON text); `documentAt` reads that text for the tree / JSON
 *   views; `documentGet` and `documentApply` read and write by `_id` (optimistic, one edit at a
 *   time, no transaction).
 * - Documents, `_id`s and field values of edits are Extended JSON **text**: keep the text the
 *   core sent and send it back unchanged (a JavaScript object would move integer-like keys
 *   first and break the optimistic check on field order).
 * - Keys: `keyScanOpen` and `keyValueOpen` start query runs (columns in the contract);
 *   `keyInfo` and `keyApply` read and write one key (atomic per request).
 * - Pub/sub: `pubsubSubscribe` streams events on a channel until `pubsubUnsubscribe`, the
 *   session closes or the panel's channel is gone.
 * - The mongo shell and the Redis console run with `queryRun` / `statementAt`
 *   (`./workbench`) like SQL; a statement that needs confirmation fails with an
 *   `invalid_input` error starting with `CONFIRMATION_REQUIRED` (`isConfirmationRequired`), and
 *   runs after the user agrees with `RunOptions.confirmDestructive`.
 */
import { Channel } from "@tauri-apps/api/core";
import type { CommandError } from "../connections/types";
import { call } from "./call";
import type { EditorId, ObjectRef, RunEvent, RunId, RunOptions } from "./types";

// --- shared ---------------------------------------------------------------------------------

/** Start of the message of a run refused until the user confirms (`oxbase_dialect`). */
export const CONFIRMATION_REQUIRED = "confirmation required: ";

/** `true` for the error of a run that needs `RunOptions.confirmDestructive`. */
export function isConfirmationRequired(error: CommandError): boolean {
  return error.category === "invalid_input" && error.message.startsWith(CONFIRMATION_REQUIRED);
}

/** Arrow field metadata of the document column (`oxbase.role = document`). */
export const DOCUMENT_COLUMN = "$document";
export const META_ROLE = "oxbase.role";
export const ROLE_DOCUMENT = "document";
/** Arrow field metadata: the BSON type of a document column (`mixed` for several). */
export const META_BSON_TYPE = "oxbase.bson_type";
export const META_BSON_TYPES = "oxbase.bson_types";
/** Arrow field metadata of `Binary` columns holding user bytes (Redis). */
export const META_ENCODING = "oxbase.encoding";

// --- documents (MongoDB) ----------------------------------------------------------------------

/** Extended JSON value (relaxed or canonical) in a query. */
export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export interface FindRequest {
  /** `kind: "table"` (collection) or `"view"`, `catalog` = the database. */
  collection: ObjectRef;
  filter?: { [key: string]: Json };
  projection?: { [key: string]: Json };
  /** Key order is the sort order. */
  sort?: { [key: string]: Json };
  skip?: number;
  limit?: number;
  hint?: string | { [key: string]: Json };
  collation?: { [key: string]: Json };
  maxTimeMs?: number;
}

export interface AggregateRequest {
  collection: ObjectRef;
  /** Stages, each `{ "$stage": ... }`; `$out` / `$merge` write (ask first). */
  pipeline: { [key: string]: Json }[];
  allowDiskUse?: boolean;
  collation?: { [key: string]: Json };
  maxTimeMs?: number;
}

/** `oxbase_types::DocumentQuery`. */
export type DocumentQuery =
  | ({ type: "find" } & FindRequest)
  | ({ type: "aggregate" } & AggregateRequest);

/** A field's value (Extended JSON text) or its absence. `"null"` is a present null. */
export type FieldValue = { state: "present"; value: string } | { state: "missing" };

export interface FieldChange {
  /** Dotted path; no empty segment, no segment starting with `$`, never `_id`. */
  path: string;
  previous: FieldValue;
  value: FieldValue;
}

/** `oxbase_types::DocumentEdit`; every text is Extended JSON. */
export type DocumentEdit =
  | { type: "replace"; id: string; previous: string; document: string }
  | { type: "update"; id: string; changes: FieldChange[] }
  | { type: "insert"; document: string }
  | { type: "delete"; id: string; previous?: string };

export type DocumentEditStatus = "applied" | "conflict" | "failed" | "notRun";

export interface DocumentEditOutcome {
  index: number;
  status: DocumentEditStatus;
  /** `_id` (canonical Extended JSON text). */
  id?: string;
  /** The stored document after an applied replace, update or insert. */
  document?: string;
  error?: string;
}

export interface DocumentApplyReport {
  outcomes: DocumentEditOutcome[];
}

function runChannel(onEvent: (event: RunEvent) => void): Channel<RunEvent> {
  const channel = new Channel<RunEvent>();
  channel.onmessage = onEvent;
  return channel;
}

/** Runs a `find` in editor `editorId`; events like `queryRun`. */
export function documentFind(
  sessionId: string,
  editorId: EditorId,
  request: FindRequest,
  onEvent: (event: RunEvent) => void,
  options?: RunOptions,
  requestId?: string,
): Promise<RunId> {
  return call("document_find", {
    sessionId,
    editorId,
    request,
    options: options ?? null,
    onEvent: runChannel(onEvent),
    requestId: requestId ?? null,
  });
}

/** Runs an `aggregate` in editor `editorId`; events like `queryRun`. */
export function documentAggregate(
  sessionId: string,
  editorId: EditorId,
  request: AggregateRequest,
  onEvent: (event: RunEvent) => void,
  options?: RunOptions,
  requestId?: string,
): Promise<RunId> {
  return call("document_aggregate", {
    sessionId,
    editorId,
    request,
    options: options ?? null,
    onEvent: runChannel(onEvent),
    requestId: requestId ?? null,
  });
}

/** The whole document (canonical Extended JSON text) of one row of a document run. */
export function documentAt(runId: RunId, resultIndex: number, row: number): Promise<string> {
  return call("document_at", { runId, resultIndex, row });
}

/** One document by `_id` (Extended JSON text); `null` when there is none. */
export function documentGet(
  sessionId: string,
  collection: ObjectRef,
  id: string,
  requestId?: string,
): Promise<string | null> {
  return call("document_get", { sessionId, collection, id, requestId: requestId ?? null });
}

/** Applies edits in order, each on its own; the first conflict or failure stops the rest. */
export function documentApply(
  sessionId: string,
  collection: ObjectRef,
  edits: DocumentEdit[],
  requestId?: string,
): Promise<DocumentApplyReport> {
  return call("document_apply", { sessionId, collection, edits, requestId: requestId ?? null });
}

/** Replaces one document if it is still `previous`. */
export function documentReplace(
  sessionId: string,
  collection: ObjectRef,
  id: string,
  previous: string,
  document: string,
): Promise<DocumentApplyReport> {
  return documentApply(sessionId, collection, [{ type: "replace", id, previous, document }]);
}

/** Inserts one document (Extended JSON text). */
export function documentInsert(
  sessionId: string,
  collection: ObjectRef,
  document: string,
): Promise<DocumentApplyReport> {
  return documentApply(sessionId, collection, [{ type: "insert", document }]);
}

/** Deletes one document (only if it is still `previous`, when given). */
export function documentDelete(
  sessionId: string,
  collection: ObjectRef,
  id: string,
  previous?: string,
): Promise<DocumentApplyReport> {
  const edit: DocumentEdit =
    previous === undefined ? { type: "delete", id } : { type: "delete", id, previous };
  return documentApply(sessionId, collection, [edit]);
}

// --- keys (Redis) -----------------------------------------------------------------------------

export type KeyType = "string" | "list" | "set" | "zset" | "hash" | "stream" | "json" | "other";

/** Bytes as JSON: UTF-8 text when valid, else lowercase hex. */
export type BinaryText = { encoding: "utf8"; text: string } | { encoding: "hex"; hex: string };

/** `text` as `BinaryText`. */
export function utf8(text: string): BinaryText {
  return { encoding: "utf8", text };
}

/**
 * `bytes` as `BinaryText`: UTF-8 when they decode, else hex. Byte for byte: a leading UTF-8
 * BOM (`EF BB BF`) stays in the text as U+FEFF (`ignoreBOM`), because the text is the key,
 * field or member that edits and removals name; dropped, they would name another one.
 */
export function binaryText(bytes: Uint8Array): BinaryText {
  try {
    return utf8(new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes));
  } catch {
    return {
      encoding: "hex",
      hex: Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join(""),
    };
  }
}

/** One line, like the core's `BinaryText::display`: text, or bytes escaped `\xHH`. */
export function displayBinary(value: BinaryText): string {
  if (value.encoding === "utf8") return value.text;
  let out = "";
  for (let i = 0; i + 1 < value.hex.length; i += 2) {
    const b = Number.parseInt(value.hex.slice(i, i + 2), 16);
    if (b === 0x5c) out += "\\\\";
    else if (b === 0x22) out += '\\"';
    else if (b === 0x0a) out += "\\n";
    else if (b === 0x0d) out += "\\r";
    else if (b === 0x09) out += "\\t";
    else if (b >= 0x20 && b <= 0x7e) out += String.fromCharCode(b);
    else out += `\\x${value.hex.slice(i, i + 2).toLowerCase()}`;
  }
  return out;
}

export interface KeyAddress {
  /** Logical database index as text (`"0"`). */
  database: string;
  key: BinaryText;
}

export interface KeyScanRequest {
  database: string;
  /** `MATCH` glob. */
  pattern?: string;
  keyType?: Exclude<KeyType, "other">;
  /** `COUNT` hint per round trip (default 1000). */
  count?: number;
  limit?: number;
}

export interface KeyInfo {
  address: KeyAddress;
  keyType: KeyType;
  ttlMs?: number;
  length?: number;
  encoding?: string;
  memoryBytes?: number;
}

export interface StreamField {
  field: BinaryText;
  value: BinaryText;
}

/** `oxbase_types::KeyEdit`. */
export type KeyEdit =
  | { type: "setString"; value: BinaryText; keepTtl?: boolean }
  | { type: "expire"; ttlMs?: number }
  | { type: "rename"; newKey: BinaryText; overwrite?: boolean }
  | { type: "delete" }
  | { type: "hashSet"; field: BinaryText; value: BinaryText; onlyNew?: boolean }
  | { type: "hashDelete"; fields: BinaryText[] }
  | { type: "listPush"; end: "head" | "tail"; values: BinaryText[] }
  | { type: "listSet"; index: number; value: BinaryText; expected?: BinaryText }
  | { type: "listRemove"; value: BinaryText; count: number }
  | { type: "setAdd"; members: BinaryText[] }
  | { type: "setRemove"; members: BinaryText[] }
  | { type: "zsetAdd"; member: BinaryText; score: number }
  | { type: "zsetRemove"; members: BinaryText[] }
  | { type: "streamAdd"; id?: string; fields: StreamField[] }
  | { type: "streamDelete"; ids: string[] }
  | { type: "jsonSet"; path: string; value: Json };

/** `true` for edits that need `confirmDestructive` (`delete`, overwriting `rename`). */
export function isDestructiveKeyEdit(edit: KeyEdit): boolean {
  return edit.type === "delete" || (edit.type === "rename" && edit.overwrite === true);
}

export interface KeyApplyRequest {
  address: KeyAddress;
  /** The type the editor read (a different type now is a conflict). */
  keyType?: KeyType;
  /** The key must not exist yet; the first edit writes its value. */
  create?: boolean;
  edits: KeyEdit[];
  confirmDestructive?: boolean;
}

export interface KeyApplyReport {
  committed: boolean;
  conflict: boolean;
  error?: string;
  /** The key afterwards (the new key after a rename; absent after a delete). */
  info?: KeyInfo;
}

export interface KeyValueOpened {
  runId: RunId;
  info: KeyInfo;
}

/** Streams every key the request matches (`SCAN`) in editor `editorId`. */
export function keyScanOpen(
  sessionId: string,
  editorId: EditorId,
  request: KeyScanRequest,
  onEvent: (event: RunEvent) => void,
  options?: RunOptions,
  requestId?: string,
): Promise<RunId> {
  return call("key_scan_open", {
    sessionId,
    editorId,
    request,
    options: options ?? null,
    onEvent: runChannel(onEvent),
    requestId: requestId ?? null,
  });
}

/** Type, TTL, length, encoding and memory of one key. */
export function keyInfo(sessionId: string, key: KeyAddress, requestId?: string): Promise<KeyInfo> {
  return call("key_info", { sessionId, key, requestId: requestId ?? null });
}

/** Streams one key's whole value in editor `editorId`; the info's type picks the editor. */
export function keyValueOpen(
  sessionId: string,
  editorId: EditorId,
  key: KeyAddress,
  onEvent: (event: RunEvent) => void,
  options?: RunOptions,
  requestId?: string,
): Promise<KeyValueOpened> {
  return call("key_value_open", {
    sessionId,
    editorId,
    key,
    options: options ?? null,
    onEvent: runChannel(onEvent),
    requestId: requestId ?? null,
  });
}

/** Applies typed edits of one key atomically. */
export function keyApply(
  sessionId: string,
  request: KeyApplyRequest,
  requestId?: string,
): Promise<KeyApplyReport> {
  return call("key_apply", { sessionId, request, requestId: requestId ?? null });
}

// --- pub/sub (Redis) --------------------------------------------------------------------------

export interface SubscribeRequest {
  channels?: BinaryText[];
  patterns?: BinaryText[];
}

/** `oxbase_dialect::PubSubEvent`, in order: subscribed, message* (dropped between), closed. */
export type PubSubEvent =
  | { type: "subscribed"; channels: BinaryText[]; patterns: BinaryText[] }
  | {
      type: "message";
      channel: BinaryText;
      pattern?: BinaryText;
      payload: BinaryText;
      receivedAtMs: number;
    }
  | { type: "dropped"; count: number }
  | { type: "closed"; error?: string };

/** Subscribes; returns the subscription id. Unsubscribe when the panel closes. */
export function pubsubSubscribe(
  sessionId: string,
  request: SubscribeRequest,
  onEvent: (event: PubSubEvent) => void,
  requestId?: string,
): Promise<string> {
  const channel = new Channel<PubSubEvent>();
  channel.onmessage = onEvent;
  return call("pubsub_subscribe", {
    sessionId,
    request,
    onEvent: channel,
    requestId: requestId ?? null,
  });
}

/** Closes a subscription; `false` when it was not open. */
export function pubsubUnsubscribe(subscriptionId: string): Promise<boolean> {
  return call("pubsub_unsubscribe", { subscriptionId });
}

/** Publishes `payload` on `channel`; returns the number of receivers. */
export function pubsubPublish(
  sessionId: string,
  channel: BinaryText,
  payload: BinaryText,
  requestId?: string,
): Promise<number> {
  return call("pubsub_publish", { sessionId, channel, payload, requestId: requestId ?? null });
}
