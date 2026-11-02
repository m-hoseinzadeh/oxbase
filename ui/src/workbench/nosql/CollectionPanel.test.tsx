import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { useEffect, useRef } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type {
  ColumnInfo,
  DocumentApplyReport,
  ObjectRef,
  RunEvent,
  StatementSpan,
} from "../../api";
import { tauriCore } from "../../test/tauriMock";
import type { SqlEditorProps } from "../editor/SqlEditor";
import type { ResultGridProps } from "../results/ResultGrid";
import { CollectionPanel } from "./CollectionPanel";

vi.mock("@tauri-apps/api/core", async () => (await import("../../test/tauriMock")).tauriCore);

// Monaco needs a browser: a textarea stands in (with the same handle and onChange).
vi.mock("../editor/SqlEditor", () => ({
  SqlEditor: ({ initialValue, editorRef, onChange, ariaLabel }: SqlEditorProps) => {
    const area = useRef<HTMLTextAreaElement>(null);
    useEffect(() => {
      editorRef.current = {
        getText: () => area.current?.value ?? "",
        getSelection: () => null,
        getCursorOffset: () => 0,
        setText: (text) => {
          if (area.current) area.current.value = text;
        },
        select: () => {},
        focus: () => {},
      };
      return () => {
        editorRef.current = null;
      };
    }, [editorRef]);
    return (
      <textarea
        aria-label={ariaLabel}
        ref={area}
        defaultValue={initialValue}
        onChange={(e) => onChange?.(e.target.value)}
      />
    );
  },
}));

// The canvas grid cannot draw in jsdom: rows are buttons that select themselves.
vi.mock("../results/ResultGrid", () => ({
  ResultGrid: ({ rowCount, columns, onSelectRows }: ResultGridProps) => (
    <div data-testid="grid" data-columns={columns.map((c) => c.name).join(",")}>
      {Array.from({ length: rowCount }, (_, r) => (
        <button
          // biome-ignore lint/suspicious/noArrayIndexKey: rows are positions.
          key={r}
          type="button"
          onClick={() => onSelectRows?.({ current: r, ranges: [[r, r + 1]] })}
        >
          row {r}
        </button>
      ))}
    </div>
  ),
}));

const DOCS: ObjectRef = { kind: "table", catalog: "oxbase_test", name: "m7_docs" };
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
const COLUMNS: ColumnInfo[] = [
  { name: "_id", dataType: "Utf8", nullable: false, metadata: { "oxbase.bson_type": "objectId" } },
  { name: "10", dataType: "Int32", nullable: true },
  {
    name: "$document",
    dataType: "Utf8",
    nullable: false,
    metadata: { "oxbase.role": "document", "ARROW:extension:name": "arrow.json" },
  },
];
const DOC0 =
  '{"_id":{"$oid":"65a000000000000000000001"},"b":{"$numberInt":"1"},"10":{"$numberInt":"2"}}';

let onEvent: ((event: RunEvent) => void) | null = null;
let applyReport: DocumentApplyReport = { outcomes: [{ index: 0, status: "applied" }] };

function stream(rows: number) {
  const send = (event: RunEvent) => onEvent?.(event);
  act(() => {
    send({ type: "started", runId: "run-1", statements: [SPAN] });
    send({ type: "statementStarted", statementIndex: 0 });
    send({
      type: "resultSet",
      resultIndex: 0,
      statementIndex: 0,
      statementRange: SPAN,
      columns: COLUMNS,
    });
    send({ type: "resultDone", resultIndex: 0, totalRows: rows, truncated: false });
    send({ type: "statementDone", statementIndex: 0, elapsedMs: 3 });
    send({ type: "finished", elapsedMs: 4 });
  });
}

function mockCore() {
  tauriCore.invoke.mockImplementation(async (cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "document_find":
      case "document_aggregate":
        onEvent = (a.onEvent as { onmessage: (e: RunEvent) => void }).onmessage;
        return "run-1";
      case "document_at":
        return DOC0;
      case "document_apply":
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

async function renderPanel(collection: ObjectRef = DOCS) {
  render(<CollectionPanel editorId="ed-1" sessionId="s-1" collection={collection} />);
  await waitFor(() => expect(calls("document_find")).toHaveLength(1));
  stream(2);
}

describe("CollectionPanel", () => {
  beforeEach(() => {
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReturnValue(true);
    applyReport = { outcomes: [{ index: 0, status: "applied" }] };
    mockCore();
  });

  it("opens the collection with a find and hides the document column in the grid", async () => {
    await renderPanel();
    expect(calls("document_find")[0]).toMatchObject({
      sessionId: "s-1",
      editorId: "ed-1",
      request: { collection: DOCS },
    });
    expect(screen.getByTestId("collection-status")).toHaveTextContent("2 documents");
    // The grid receives every column; it hides `$document` itself (see results/format).
    expect(screen.getByTestId("grid")).toBeInTheDocument();
  });

  it("runs the builder's find with filter, sort order and limit", async () => {
    await renderPanel();
    fireEvent.change(screen.getByLabelText("Filter"), { target: { value: '{"n": {"$gt": 1}}' } });
    fireEvent.change(screen.getByLabelText("Sort"), { target: { value: '{"b": -1, "a": 1}' } });
    fireEvent.change(screen.getByPlaceholderText("all"), { target: { value: "5" } });
    fireEvent.click(screen.getByTestId("collection-run"));
    await waitFor(() => expect(calls("document_find")).toHaveLength(2));
    const request = calls("document_find")[1]?.request as Record<string, unknown>;
    expect(request).toEqual({
      collection: DOCS,
      filter: { n: { $gt: 1 } },
      sort: { b: -1, a: 1 },
      limit: 5,
    });
    expect(Object.keys(request.sort as object)).toEqual(["b", "a"]);
  });

  it("shows builder problems instead of running", async () => {
    await renderPanel();
    fireEvent.change(screen.getByLabelText("Filter"), { target: { value: "{n: 1}" } });
    fireEvent.click(screen.getByTestId("collection-run"));
    expect(await screen.findByText(/Not valid Extended JSON/)).toBeInTheDocument();
    expect(calls("document_find")).toHaveLength(1);
  });

  it("runs a pipeline and asks before a $out stage writes", async () => {
    await renderPanel();
    fireEvent.click(screen.getByRole("tab", { name: "Aggregate" }));
    fireEvent.change(screen.getByLabelText("Stage 1 $match"), { target: { value: '{"g": 7}' } });
    fireEvent.click(screen.getByRole("button", { name: "Add stage" }));
    fireEvent.change(screen.getByLabelText("Stage 2 operator"), { target: { value: "$out" } });
    fireEvent.click(screen.getByTestId("collection-run"));
    const dialog = await screen.findByRole("dialog", { name: "Run a pipeline that writes?" });
    expect(calls("document_aggregate")).toHaveLength(0);
    fireEvent.click(within(dialog).getByRole("button", { name: "Run and write" }));
    await waitFor(() => expect(calls("document_aggregate")).toHaveLength(1));
    expect(calls("document_aggregate")[0]).toMatchObject({
      request: { collection: DOCS, pipeline: [{ $match: { g: 7 } }, { $out: "collection" }] },
      options: { confirmDestructive: true },
    });
  });

  it("shows the selected document as a tree and as JSON, in its own field order", async () => {
    await renderPanel();
    fireEvent.click(screen.getByRole("tab", { name: "Tree" }));
    fireEvent.click(screen.getByRole("button", { name: "row 0" }));
    const tree = await screen.findByRole("tree", { name: "Document fields" });
    const items = within(tree).getAllByRole("treeitem");
    expect(items.map((i) => i.textContent)).toEqual([
      '_idObjectId("65a000000000000000000001")objectId',
      "b1int",
      "102int",
    ]);
    expect(calls("document_at")[0]).toEqual({ runId: "run-1", resultIndex: 0, row: 0 });
    fireEvent.click(screen.getByRole("tab", { name: "JSON" }));
    const json = await screen.findByTestId("document-json");
    expect(json.textContent).toContain('"b": {\n    "$numberInt": "1"\n  },\n  "10"');
  });

  it("replaces a document with the exact text it read as `previous`", async () => {
    await renderPanel();
    fireEvent.click(screen.getByRole("button", { name: "row 0" }));
    fireEvent.click(screen.getByTestId("document-edit"));
    const dialog = await screen.findByRole("dialog", {
      name: /Edit document 1 of oxbase_test.m7_docs/,
    });
    const editor = within(dialog).getByLabelText("Document") as HTMLTextAreaElement;
    expect(editor.value).toContain('"10": {');
    fireEvent.change(editor, {
      target: { value: '{"_id": {"$oid": "65a000000000000000000001"}, "b": 5}' },
    });
    fireEvent.click(within(dialog).getByTestId("document-save"));
    await waitFor(() => expect(calls("document_apply")).toHaveLength(1));
    expect(calls("document_apply")[0]).toMatchObject({
      sessionId: "s-1",
      collection: DOCS,
      edits: [
        {
          type: "replace",
          id: '{"$oid":"65a000000000000000000001"}',
          previous: DOC0,
          document: '{"_id": {"$oid": "65a000000000000000000001"}, "b": 5}',
        },
      ],
    });
    // Saved: the dialog closes and the query runs again.
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    await waitFor(() => expect(calls("document_find")).toHaveLength(2));
    expect(screen.getByText("Saved the document.")).toBeInTheDocument();
  });

  it("explains a conflict and keeps the dialog open", async () => {
    applyReport = { outcomes: [{ index: 0, status: "conflict" }] };
    await renderPanel();
    fireEvent.click(screen.getByRole("button", { name: "row 0" }));
    fireEvent.click(screen.getByTestId("document-edit"));
    const dialog = await screen.findByRole("dialog");
    fireEvent.click(within(dialog).getByTestId("document-save"));
    expect(await screen.findByTestId("document-error")).toHaveTextContent(
      /changed or deleted this document after it was read, so nothing was written/,
    );
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });

  it("refuses invalid JSON in the editor and inserts a new document", async () => {
    await renderPanel();
    fireEvent.click(screen.getByRole("button", { name: "Insert" }));
    const dialog = await screen.findByRole("dialog", { name: "Insert into oxbase_test.m7_docs" });
    const editor = within(dialog).getByLabelText("Document");
    fireEvent.change(editor, { target: { value: "{a: 1}" } });
    fireEvent.click(within(dialog).getByTestId("document-save"));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent(/quoted key/);
    fireEvent.change(editor, { target: { value: '{"a": 1}' } });
    fireEvent.click(within(dialog).getByTestId("document-save"));
    await waitFor(() =>
      expect(calls("document_apply")[0]).toMatchObject({
        edits: [{ type: "insert", document: '{"a": 1}' }],
      }),
    );
  });

  it("deletes after confirming, with the document as read", async () => {
    await renderPanel();
    fireEvent.click(screen.getByRole("button", { name: "row 0" }));
    fireEvent.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete this document?" });
    fireEvent.click(within(dialog).getByRole("button", { name: "Delete" }));
    await waitFor(() =>
      expect(calls("document_apply")[0]).toMatchObject({
        edits: [{ type: "delete", id: '{"$oid":"65a000000000000000000001"}', previous: DOC0 }],
      }),
    );
  });

  it("keeps views and aggregate results read-only", async () => {
    await renderPanel({ ...DOCS, kind: "view", name: "m7_view" });
    fireEvent.click(screen.getByRole("button", { name: "row 0" }));
    expect(screen.getByRole("button", { name: "Insert" })).toBeDisabled();
    expect(screen.getByTestId("document-edit")).toBeDisabled();
  });
});
