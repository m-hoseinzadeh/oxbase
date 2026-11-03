import { beforeEach, describe, expect, it, vi } from "vitest";

const core = vi.hoisted(() => {
  class Channel<T> {
    onmessage: (message: T) => void = () => {};
  }
  return {
    Channel,
    invoke: vi.fn<(cmd: string, args?: unknown) => Promise<unknown>>(),
    isTauri: vi.fn<() => boolean>(() => true),
  };
});
vi.mock("@tauri-apps/api/core", () => core);

import {
  binaryText,
  displayBinary,
  documentApply,
  documentAt,
  documentDelete,
  documentFind,
  isConfirmationRequired,
  isDestructiveKeyEdit,
  keyApply,
  keyScanOpen,
  keyValueOpen,
  type PubSubEvent,
  pubsubPublish,
  pubsubSubscribe,
  pubsubUnsubscribe,
  utf8,
} from "./nosql";
import type { RunEvent } from "./types";

const collection = { kind: "table" as const, catalog: "oxbase_test", name: "m7_docs" };

describe("M7 command wrappers", () => {
  beforeEach(() => {
    core.invoke.mockReset();
    core.isTauri.mockReturnValue(true);
  });

  it("starts document runs with a channel and explicit nulls", async () => {
    core.invoke.mockResolvedValue("run-1");
    const seen: RunEvent[] = [];
    const id = await documentFind(
      "s1",
      "tab-1",
      { collection, filter: { n: { $gt: 1 } }, limit: 10 },
      (e) => seen.push(e),
    );
    expect(id).toBe("run-1");
    const [cmd, args] = core.invoke.mock.calls[0] as [string, Record<string, unknown>];
    expect(cmd).toBe("document_find");
    expect(args).toMatchObject({
      sessionId: "s1",
      editorId: "tab-1",
      request: { collection, filter: { n: { $gt: 1 } }, limit: 10 },
      options: null,
      requestId: null,
    });
    const channel = args.onEvent as { onmessage: (e: RunEvent) => void };
    channel.onmessage({ type: "finished", elapsedMs: 3 });
    expect(seen).toEqual([{ type: "finished", elapsedMs: 3 }]);
  });

  it("sends document edits as Extended JSON text", async () => {
    core.invoke.mockResolvedValue({ outcomes: [] });
    await documentApply("s1", collection, [
      { type: "update", id: '{"$oid":"65a000000000000000000001"}', changes: [] },
    ]);
    expect(core.invoke).toHaveBeenLastCalledWith("document_apply", {
      sessionId: "s1",
      collection,
      edits: [{ type: "update", id: '{"$oid":"65a000000000000000000001"}', changes: [] }],
      requestId: null,
    });
    await documentDelete("s1", collection, "7");
    expect(core.invoke).toHaveBeenLastCalledWith("document_apply", {
      sessionId: "s1",
      collection,
      edits: [{ type: "delete", id: "7" }],
      requestId: null,
    });
    core.invoke.mockResolvedValue('{"_id":7}');
    expect(await documentAt("run-1", 0, 4)).toBe('{"_id":7}');
    expect(core.invoke).toHaveBeenLastCalledWith("document_at", {
      runId: "run-1",
      resultIndex: 0,
      row: 4,
    });
  });

  it("wraps key commands", async () => {
    core.invoke.mockResolvedValue("run-2");
    await keyScanOpen("s1", "keys", { database: "0", pattern: "m7:*" }, () => {}, { maxRows: 5 });
    const [scanCmd, scanArgs] = core.invoke.mock.calls[0] as [string, Record<string, unknown>];
    expect(scanCmd).toBe("key_scan_open");
    expect(scanArgs).toMatchObject({
      request: { database: "0", pattern: "m7:*" },
      options: { maxRows: 5 },
    });
    const key = { database: "0", key: utf8("m7:hash") };
    core.invoke.mockResolvedValue({ runId: "run-3", info: { address: key, keyType: "hash" } });
    const opened = await keyValueOpen("s1", "value", key, () => {});
    expect(opened.info.keyType).toBe("hash");
    core.invoke.mockResolvedValue({ committed: true, conflict: false });
    const edits = [{ type: "delete" as const }];
    await keyApply("s1", { address: key, edits, confirmDestructive: true });
    expect(core.invoke).toHaveBeenLastCalledWith("key_apply", {
      sessionId: "s1",
      request: { address: key, edits, confirmDestructive: true },
      requestId: null,
    });
    expect(isDestructiveKeyEdit({ type: "delete" })).toBe(true);
    expect(isDestructiveKeyEdit({ type: "rename", newKey: utf8("b") })).toBe(false);
    expect(isDestructiveKeyEdit({ type: "rename", newKey: utf8("b"), overwrite: true })).toBe(true);
  });

  it("streams pub/sub events and publishes", async () => {
    core.invoke.mockResolvedValue("sub-1");
    const seen: PubSubEvent[] = [];
    const id = await pubsubSubscribe("s1", { channels: [utf8("news")] }, (e) => seen.push(e));
    expect(id).toBe("sub-1");
    const [, args] = core.invoke.mock.calls[0] as [string, Record<string, unknown>];
    (args.onEvent as { onmessage: (e: PubSubEvent) => void }).onmessage({
      type: "dropped",
      count: 2,
    });
    expect(seen).toEqual([{ type: "dropped", count: 2 }]);
    core.invoke.mockResolvedValue(true);
    expect(await pubsubUnsubscribe("sub-1")).toBe(true);
    expect(core.invoke).toHaveBeenLastCalledWith("pubsub_unsubscribe", { subscriptionId: "sub-1" });
    core.invoke.mockResolvedValue(1);
    expect(await pubsubPublish("s1", utf8("news"), utf8("hi"))).toBe(1);
    expect(core.invoke).toHaveBeenLastCalledWith("pubsub_publish", {
      sessionId: "s1",
      channel: utf8("news"),
      payload: utf8("hi"),
      requestId: null,
    });
  });
});

describe("M7 helpers", () => {
  it("encodes bytes like the core", () => {
    expect(binaryText(new TextEncoder().encode("grüße"))).toEqual(utf8("grüße"));
    const bin = binaryText(new Uint8Array([0x00, 0xff, 0x61, 0x22, 0x5c, 0x0a]));
    expect(bin).toEqual({ encoding: "hex", hex: "00ff61225c0a" });
    expect(displayBinary(bin)).toBe('\\x00\\xffa\\"\\\\\\n');
    expect(displayBinary(utf8("plain"))).toBe("plain");
  });

  it("recognizes the confirmation error", () => {
    expect(
      isConfirmationRequired({
        category: "invalid_input",
        message: "confirmation required: statement 1: x",
      }),
    ).toBe(true);
    expect(isConfirmationRequired({ category: "invalid_input", message: "bad" })).toBe(false);
    expect(isConfirmationRequired({ category: "sql", message: "confirmation required: x" })).toBe(
      false,
    );
  });
});
