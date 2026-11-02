import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import {
  Binary,
  Field,
  Int64,
  Schema,
  Table,
  tableToIPC,
  Utf8,
  vectorFromArray,
} from "apache-arrow";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type {
  ColumnInfo,
  KeyApplyReport,
  KeyInfo,
  RunEvent,
  StatementSpan,
  TreeNode,
} from "../../../api";
import { tauriCore } from "../../../test/tauriMock";
import type { ResultGridProps } from "../../results/ResultGrid";
import { KeyBrowserPanel } from "./KeyBrowserPanel";

vi.mock("@tauri-apps/api/core", async () => (await import("../../../test/tauriMock")).tauriCore);

vi.mock("../../results/ResultGrid", () => ({
  ResultGrid: ({ rowCount, onSelectRows }: ResultGridProps) => (
    <div data-testid="value-grid">
      {Array.from({ length: rowCount }, (_, r) => (
        <button
          // biome-ignore lint/suspicious/noArrayIndexKey: rows are positions.
          key={r}
          type="button"
          onClick={() => onSelectRows?.({ current: r, ranges: [[r, r + 1]] })}
        >
          element {r}
        </button>
      ))}
    </div>
  ),
}));

const SPAN: StatementSpan = {
  index: 0,
  startByte: 0,
  endByte: 1,
  startLine: 1,
  startColumn: 1,
  endLine: 1,
  endColumn: 2,
  returnsRows: true,
};
const BIN: Record<string, string> = { "oxbase.encoding": "binary" };
const SCAN_COLUMNS: ColumnInfo[] = [
  { name: "key", dataType: "Binary", nullable: false, metadata: BIN },
  { name: "key_text", dataType: "Utf8", nullable: false },
  { name: "type", dataType: "Utf8", nullable: false },
  { name: "ttl_ms", dataType: "Int64", nullable: true },
];
const HASH_COLUMNS: ColumnInfo[] = [
  { name: "field", dataType: "Binary", nullable: false, metadata: BIN },
  { name: "value", dataType: "Binary", nullable: false, metadata: BIN },
];

const enc = (s: string) => new TextEncoder().encode(s);

function scanPage(): Table {
  const keys = ["m7:hash", "m7:string", "m7:binary"];
  const table = new Table(
    new Schema([
      new Field("key", new Binary(), false),
      new Field("key_text", new Utf8(), false),
      new Field("type", new Utf8(), false),
      new Field("ttl_ms", new Int64(), true),
    ]),
    {
      key: vectorFromArray(
        [enc("m7:hash"), enc("m7:string"), Uint8Array.from([0x6d, 0xff])],
        new Binary(),
      ),
      key_text: vectorFromArray([keys[0], keys[1], "m\\xff"], new Utf8()),
      type: vectorFromArray(["hash", "string", "string"], new Utf8()),
      ttl_ms: vectorFromArray([null, 90_000n, null], new Int64()),
    },
  );
  return table;
}

function hashPage(): Table {
  const table = new Table(
    new Schema([new Field("field", new Binary(), false), new Field("value", new Binary(), false)]),
    {
      field: vectorFromArray([enc("name"), enc("n")], new Binary()),
      value: vectorFromArray([enc("Ada"), enc("1")], new Binary()),
    },
  );
  return table;
}

function stringPage(): Table {
  const table = new Table(new Schema([new Field("value", new Binary(), false)]), {
    value: vectorFromArray([enc("hello")], new Binary()),
  });
  return table;
}

const listeners = new Map<string, (event: RunEvent) => void>();
let applyReport: KeyApplyReport = { committed: true, conflict: false };

function info(key: string, keyType: KeyInfo["keyType"], length: number): KeyInfo {
  return { address: { database: "0", key: { encoding: "utf8", text: key } }, keyType, length };
}

function stream(runId: string, columns: ColumnInfo[], rows: number) {
  const send = (event: RunEvent) => listeners.get(runId)?.(event);
  act(() => {
    send({ type: "started", runId, statements: [SPAN] });
    send({ type: "statementStarted", statementIndex: 0 });
    send({ type: "resultSet", resultIndex: 0, statementIndex: 0, statementRange: SPAN, columns });
    send({ type: "resultDone", resultIndex: 0, totalRows: rows, truncated: false });
    send({ type: "statementDone", statementIndex: 0, elapsedMs: 1 });
    send({ type: "finished", elapsedMs: 1 });
  });
}

const NAMESPACE: TreeNode = {
  node: { type: "object", object: { kind: "schema", catalog: "0", name: "m7:" } },
  key: "ns:m7:",
  label: "m7:",
  expandable: true,
};

let scans = 0;
let valueRun = "";

function mockCore() {
  tauriCore.invoke.mockImplementation(async (cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    const channel = a.onEvent as { onmessage: (e: RunEvent) => void } | undefined;
    switch (cmd) {
      case "list_open_connections":
        return [];
      case "tree_children": {
        const node = a.node as { type: string; object?: { kind: string } };
        if (node.type === "connection") {
          return [0, 1].map((n) => ({
            node: { type: "object", object: { kind: "database", name: String(n) } },
            key: `db:${n}`,
            label: `db${n}`,
            expandable: true,
          }));
        }
        return node.object?.kind === "database" ? [NAMESPACE] : [];
      }
      case "key_scan_open": {
        scans += 1;
        const id = `scan-${scans}`;
        if (channel) listeners.set(id, channel.onmessage);
        return id;
      }
      case "key_value_open": {
        const key = (a.key as { key: { text: string } }).key.text;
        valueRun = `value-${key}`;
        if (channel) listeners.set(valueRun, channel.onmessage);
        return {
          runId: valueRun,
          info: key === "m7:hash" ? info(key, "hash", 2) : info(key, "string", 5),
        };
      }
      case "key_info": {
        const key = (a.key as { key: { encoding: string; text?: string } }).key;
        return info(key.text ?? "bin", "string", 3);
      }
      case "query_page": {
        const table =
          a.runId === valueRun
            ? valueRun === "value-m7:hash"
              ? hashPage()
              : stringPage()
            : scanPage();
        const offset = a.offset as number;
        return tableToIPC(table.slice(offset, offset + (a.limit as number)), "stream");
      }
      case "key_apply":
        return applyReport;
      case "query_close_editor":
      case "query_cancel":
        return null;
      default:
        throw new Error(`unexpected command ${cmd}`);
    }
  });
}

function calls(cmd: string) {
  return tauriCore.invoke.mock.calls
    .filter(([c]) => c === cmd)
    .map(([, args]) => args as Record<string, unknown>);
}

async function renderBrowser() {
  render(<KeyBrowserPanel editorId="kb" sessionId="s-1" database="0" />);
  await waitFor(() => expect(calls("key_scan_open")).toHaveLength(1));
  stream("scan-1", SCAN_COLUMNS, 3);
  return screen.findByRole("listbox", { name: "Keys" });
}

async function openHash() {
  const list = await renderBrowser();
  const row = await within(list).findByRole("option", { name: /m7:hash/ });
  fireEvent.click(row);
  await waitFor(() => expect(calls("key_value_open")).toHaveLength(1));
  stream("value-m7:hash", HASH_COLUMNS, 2);
  return screen.findByTestId("key-editor");
}

describe("KeyBrowserPanel", () => {
  beforeEach(() => {
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReturnValue(true);
    listeners.clear();
    scans = 0;
    valueRun = "";
    applyReport = { committed: true, conflict: false };
    mockCore();
  });

  it("scans with SCAN (pattern and type filters) and lists keys with type badges and TTLs", async () => {
    const list = await renderBrowser();
    expect(calls("key_scan_open")[0]).toMatchObject({
      sessionId: "s-1",
      editorId: "kb",
      request: { database: "0" },
    });
    const options = await within(list).findAllByRole("option");
    expect(options).toHaveLength(3);
    expect(options[0]).toHaveTextContent("HASH");
    expect(options[0]).toHaveTextContent("m7:hash");
    expect(options[1]).toHaveTextContent("1 min");
    // Non-UTF-8 keys show escaped bytes.
    expect(options[2]).toHaveTextContent("m\\xff");
    expect(screen.getByTestId("key-status")).toHaveTextContent("3 keys");
    // Sizes of the keys on screen are read with key_info.
    await waitFor(() => expect(calls("key_info").length).toBeGreaterThan(0), { timeout: 2000 });

    fireEvent.change(screen.getByTestId("key-pattern"), { target: { value: "m7:*" } });
    fireEvent.change(screen.getByLabelText("Key type"), { target: { value: "hash" } });
    await waitFor(() => expect(calls("key_scan_open")).toHaveLength(2));
    expect(calls("key_scan_open")[1]?.request).toEqual({
      database: "0",
      pattern: "m7:*",
      keyType: "hash",
    });
  });

  it("browses a namespace with its prefix pattern", async () => {
    await renderBrowser();
    fireEvent.click(await screen.findByRole("button", { name: "m7:" }));
    await waitFor(() => expect(calls("key_scan_open")).toHaveLength(2));
    expect(calls("key_scan_open")[1]?.request).toEqual({ database: "0", pattern: "m7:*" });
  });

  it("opens a hash in the typed editor and saves a field atomically", async () => {
    const editor = await openHash();
    expect(within(editor).getByTestId("key-meta")).toHaveTextContent("Hash");
    expect(within(editor).getByTestId("key-meta")).toHaveTextContent("2 fields");
    expect(calls("key_value_open")[0]).toMatchObject({
      editorId: "kb-value",
      key: { database: "0", key: { encoding: "utf8", text: "m7:hash" } },
    });
    fireEvent.click(await within(editor).findByRole("button", { name: "element 0" }));
    await waitFor(() => expect(within(editor).getByRole("button", { name: "Edit" })).toBeEnabled());
    fireEvent.click(within(editor).getByRole("button", { name: "Edit" }));
    const form = within(editor).getByTestId("element-form");
    expect(within(form).getByLabelText("Field")).toHaveValue("name");
    fireEvent.change(within(form).getByLabelText("Value"), { target: { value: "Grace" } });
    fireEvent.click(within(form).getByTestId("element-save"));
    await waitFor(() => expect(calls("key_apply")).toHaveLength(1));
    expect(calls("key_apply")[0]).toMatchObject({
      sessionId: "s-1",
      request: {
        address: { database: "0", key: { encoding: "utf8", text: "m7:hash" } },
        keyType: "hash",
        edits: [
          {
            type: "hashSet",
            field: { encoding: "utf8", text: "name" },
            value: { encoding: "utf8", text: "Grace" },
          },
        ],
      },
    });
    // The value is read again after the edit.
    await waitFor(() => expect(calls("key_value_open")).toHaveLength(2));
  });

  it("explains a conflict", async () => {
    applyReport = { committed: false, conflict: true };
    const editor = await openHash();
    fireEvent.click(await within(editor).findByRole("button", { name: "element 1" }));
    await waitFor(() =>
      expect(within(editor).getByRole("button", { name: "Remove" })).toBeEnabled(),
    );
    fireEvent.click(within(editor).getByRole("button", { name: "Remove" }));
    expect(await within(editor).findByTestId("key-error")).toHaveTextContent(
      /changed this key \(or its type\) after it was read, so nothing was written/,
    );
    expect(calls("key_apply")[0]).toMatchObject({
      request: { edits: [{ type: "hashDelete", fields: [{ encoding: "utf8", text: "n" }] }] },
    });
  });

  it("deletes a key only after confirming (confirmDestructive)", async () => {
    const editor = await openHash();
    fireEvent.click(within(editor).getByTestId("key-delete"));
    const dialog = await screen.findByRole("dialog", { name: "Delete this key?" });
    expect(calls("key_apply")).toHaveLength(0);
    fireEvent.click(within(dialog).getByTestId("confirm-action"));
    await waitFor(() => expect(calls("key_apply")).toHaveLength(1));
    expect(calls("key_apply")[0]).toMatchObject({
      request: { edits: [{ type: "delete" }], confirmDestructive: true },
    });
    // The list is scanned again and the editor closes.
    await waitFor(() => expect(calls("key_scan_open")).toHaveLength(2));
    expect(screen.getByText("No key selected")).toBeInTheDocument();
  });

  it("edits a string value and sets a TTL", async () => {
    const list = await renderBrowser();
    fireEvent.click(await within(list).findByRole("option", { name: /m7:string/ }));
    await waitFor(() => expect(calls("key_value_open")).toHaveLength(1));
    stream(
      "value-m7:string",
      [{ name: "value", dataType: "Binary", nullable: false, metadata: BIN }],
      1,
    );
    const value = await screen.findByTestId("string-value");
    await waitFor(() => expect(value).toHaveValue("hello"));
    fireEvent.change(value, { target: { value: "hello world" } });
    fireEvent.click(screen.getByTestId("string-save"));
    await waitFor(() => expect(calls("key_apply")).toHaveLength(1));
    expect(calls("key_apply")[0]).toMatchObject({
      request: {
        keyType: "string",
        edits: [
          { type: "setString", value: { encoding: "utf8", text: "hello world" }, keepTtl: true },
        ],
      },
    });

    // The value is read again after the save.
    await waitFor(() => expect(calls("key_value_open")).toHaveLength(2));
    stream(
      "value-m7:string",
      [{ name: "value", dataType: "Binary", nullable: false, metadata: BIN }],
      1,
    );
    const ttl = await screen.findByRole("button", { name: "TTL…" });
    await waitFor(() => expect(ttl).toBeEnabled());
    fireEvent.click(ttl);
    const dialog = await screen.findByRole("dialog", { name: "Time to live" });
    fireEvent.change(within(dialog).getByLabelText("Expire after (seconds)"), {
      target: { value: "60" },
    });
    fireEvent.click(within(dialog).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls("key_apply")).toHaveLength(2));
    expect(calls("key_apply")[1]).toMatchObject({
      request: { edits: [{ type: "expire", ttlMs: 60_000 }] },
    });
  });

  it("creates a key that must not exist yet", async () => {
    await renderBrowser();
    fireEvent.click(screen.getByRole("button", { name: "New key" }));
    const dialog = await screen.findByRole("dialog", { name: "New key in database 0" });
    fireEvent.change(within(dialog).getByLabelText("Key name"), { target: { value: "m7:new" } });
    fireEvent.change(within(dialog).getByLabelText("Type"), { target: { value: "set" } });
    fireEvent.change(within(dialog).getByLabelText("First member"), { target: { value: "a" } });
    fireEvent.click(within(dialog).getByTestId("new-key-create"));
    await waitFor(() => expect(calls("key_apply")).toHaveLength(1));
    expect(calls("key_apply")[0]).toMatchObject({
      request: {
        address: { database: "0", key: { encoding: "utf8", text: "m7:new" } },
        create: true,
        edits: [{ type: "setAdd", members: [{ encoding: "utf8", text: "a" }] }],
      },
    });
  });
});
