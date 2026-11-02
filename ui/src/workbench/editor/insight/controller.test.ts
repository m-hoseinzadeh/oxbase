import { afterEach, beforeEach, describe, expect, it, type Mock, vi } from "vitest";
import type {
  CompletionItem,
  CompletionResponse,
  DiagnosticItem,
  DiagnosticsResponse,
  InsightRequest,
} from "../../../api";
import { InsightSequencer } from "../../../api";
import { ENUMS, FakeEditor, FakeModel, FakeToken } from "../../../test/fakeMonaco";
import { tauriCore } from "../../../test/tauriMock";
import { InsightController, type InsightTarget } from "./controller";
import type { MarkerLike } from "./mapping";

vi.mock("@tauri-apps/api/core", async () => (await import("../../../test/tauriMock")).tauriCore);

/** A promise the test settles by hand. */
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

type Args = Record<string, unknown>;

/** Every pending `invoke` of a command, in call order, for the test to answer. */
const pending = new Map<string, { args: Args; answer: ReturnType<typeof deferred<unknown>> }[]>();

function answerNext(cmd: string, value: unknown) {
  const queue = pending.get(cmd) ?? [];
  const next = queue.shift();
  if (!next) throw new Error(`no pending ${cmd}`);
  next.answer.resolve(value);
}

function failNext(cmd: string, error: unknown) {
  const next = (pending.get(cmd) ?? []).shift();
  if (!next) throw new Error(`no pending ${cmd}`);
  next.answer.reject(error);
}

function calls(cmd: string): Args[] {
  return tauriCore.invoke.mock.calls
    .filter(([c]) => c === cmd)
    .map(([, args]) => (args ?? {}) as Args);
}

/** Lets awaited promise chains settle. */
async function flush() {
  for (let i = 0; i < 5; i += 1) await Promise.resolve();
}

function item(
  label: string,
  kind: CompletionItem["kind"],
  start: number,
  end: number,
): CompletionItem {
  return {
    label,
    kind,
    insertText: kind === "function" ? `${label}($0)` : label,
    isSnippet: kind === "function",
    detail: kind === "table" ? "table · public" : null,
    documentation: kind === "table" ? "Customer master data." : null,
    sortText: `1-${label}`,
    filterText: null,
    range: { start, end },
  };
}

function completion(seq: number, items: CompletionItem[], extra: Partial<CompletionResponse> = {}) {
  return {
    seq,
    superseded: false,
    items,
    incomplete: false,
    cache: "ready",
    elapsedUs: 900,
    ...extra,
  } satisfies CompletionResponse;
}

function diagnostics(seq: number, list: DiagnosticItem[]): DiagnosticsResponse {
  return { seq, superseded: false, diagnostics: list, cache: "ready", elapsedUs: 300 };
}

const TARGET: InsightTarget = { engine: "postgres", sessionId: "s-1", database: "shop" };

let target: InsightTarget | null;
let model: FakeModel;
let markers: { set: Mock<(markers: MarkerLike[]) => void>; clear: Mock<() => void> };
let cacheStates: string[];

function controller(editorId = "editor-1") {
  return new InsightController({
    editorId,
    target: () => target,
    model: () => model,
    enums: ENUMS,
    markers,
    sequencer: new InsightSequencer(),
    onCacheState: (state) => cacheStates.push(state),
  });
}

beforeEach(() => {
  pending.clear();
  tauriCore.invoke.mockReset();
  tauriCore.isTauri.mockReturnValue(true);
  tauriCore.invoke.mockImplementation((cmd, args) => {
    const answer = deferred<unknown>();
    const queue = pending.get(cmd) ?? [];
    queue.push({ args: (args ?? {}) as Args, answer });
    pending.set(cmd, queue);
    return answer.promise;
  });
  target = { ...TARGET };
  model = new FakeModel("SELECT * FROM cu");
  markers = { set: vi.fn<(markers: MarkerLike[]) => void>(), clear: vi.fn<() => void>() };
  cacheStates = [];
});

afterEach(() => {
  vi.useRealTimers();
});

describe("completion", () => {
  it("sends the editor's request and maps the answer to Monaco items", async () => {
    const c = controller();
    const result = c.complete(model, { lineNumber: 1, column: 17 }, new FakeToken());
    await flush();
    const [args] = calls("insight_complete");
    const request = args?.request as InsightRequest;
    expect(request).toEqual({
      editorId: "editor-1",
      seq: 1,
      engine: "postgres",
      sessionId: "s-1",
      database: "shop",
      text: "SELECT * FROM cu",
      offset: 16,
    });
    expect(args?.limit).toBe(200);
    expect(typeof args?.requestId).toBe("string");
    answerNext(
      "insight_complete",
      completion(1, [item("customers", "table", 14, 16), item("count", "function", 14, 16)], {
        incomplete: true,
      }),
    );
    const list = await result;
    expect(list.incomplete).toBe(true);
    expect(list.suggestions).toHaveLength(2);
    const [table, fn] = list.suggestions;
    expect(table).toMatchObject({
      label: { label: "customers", description: "table · public" },
      kind: ENUMS.CompletionItemKind.Struct,
      insertText: "customers",
      detail: "table · public",
      documentation: { value: "Customer master data." },
      sortText: "1-customers",
      range: {
        insert: { startLineNumber: 1, startColumn: 15, endLineNumber: 1, endColumn: 17 },
        replace: { startLineNumber: 1, startColumn: 15, endLineNumber: 1, endColumn: 17 },
      },
    });
    expect(table?.insertTextRules).toBeUndefined();
    expect(fn).toMatchObject({
      kind: ENUMS.CompletionItemKind.Function,
      insertText: "count($0)",
      insertTextRules: ENUMS.CompletionItemInsertTextRule.InsertAsSnippet,
    });
    expect(cacheStates).toEqual(["ready"]);
  });

  it("drops a stale answer when a newer request of the editor started", async () => {
    const c = controller();
    const first = c.complete(model, { lineNumber: 1, column: 16 }, new FakeToken());
    const second = c.complete(model, { lineNumber: 1, column: 17 }, new FakeToken());
    await flush();
    const seqs = calls("insight_complete").map((a) => (a.request as InsightRequest).seq);
    expect(seqs).toEqual([1, 2]);
    // The older answer arrives last: it must not replace the newer list.
    answerNext("insight_complete", completion(1, [item("cu_old", "table", 14, 15)]));
    answerNext("insight_complete", completion(2, [item("customers", "table", 14, 16)]));
    const [a, b] = await Promise.all([first, second]);
    expect(a).toEqual({ suggestions: [], incomplete: true });
    expect(b.suggestions.map((s) => s.insertText)).toEqual(["customers"]);
    expect(c.stats.droppedCompletions).toBe(1);
  });

  it("drops answers the core marks superseded", async () => {
    const c = controller();
    const superseded = c.complete(model, { lineNumber: 1, column: 17 }, new FakeToken());
    await flush();
    answerNext("insight_complete", completion(1, [], { superseded: true }));
    expect(await superseded).toEqual({ suggestions: [], incomplete: true });
    expect(c.stats.droppedCompletions).toBe(1);
  });

  it("keeps an answer that arrives after the user typed on, ranged in the request's text", async () => {
    const c = controller();
    model.setValue("SELECT 1;\nSELECT * FROM cu");
    const edited = c.complete(model, { lineNumber: 2, column: 17 }, new FakeToken());
    await flush();
    // Typing goes on while the core answers; Monaco filters the list by what was typed since.
    model.setValue("SELECT 1;\nSELECT * FROM cust");
    answerNext("insight_complete", completion(1, [item("customers", "table", 24, 26)]));
    const list = await edited;
    expect(list.suggestions.map((s) => s.insertText)).toEqual(["customers"]);
    // Monaco reads the range against the text at the request's position (column 17).
    expect(list.suggestions[0]?.range.insert).toEqual({
      startLineNumber: 2,
      startColumn: 15,
      endLineNumber: 2,
      endColumn: 17,
    });
    expect(c.stats.droppedCompletions).toBe(0);
  });

  it("cancels the core request when Monaco cancels its token", async () => {
    const c = controller();
    const token = new FakeToken();
    const result = c.complete(model, { lineNumber: 1, column: 17 }, token);
    await flush();
    const requestId = calls("insight_complete")[0]?.requestId;
    token.cancel();
    await flush();
    expect(calls("cancel_request")).toEqual([{ requestId }]);
    answerNext("cancel_request", true);
    answerNext("insight_complete", completion(1, [item("customers", "table", 14, 16)]));
    expect((await result).suggestions).toEqual([]);
  });

  it("passes every kind and the core's incomplete flag through", async () => {
    const c = controller();
    model.setValue("SELECT * FROM t WHERE a = 1 A");
    const result = c.complete(model, { lineNumber: 1, column: 30 }, new FakeToken());
    await flush();
    answerNext(
      "insight_complete",
      completion(
        1,
        [
          item("AND", "keyword", 28, 29),
          item("ABS", "function", 28, 29),
          item("amount", "column", 28, 29),
        ],
        { incomplete: false },
      ),
    );
    const list = await result;
    expect(list.suggestions.map((s) => s.insertText)).toEqual(["AND", "ABS($0)", "amount"]);
    expect(list.incomplete).toBe(false);
  });

  it("offers nothing without an engine or on errors", async () => {
    const c = controller();
    model.setValue("SELECT * FROM ");
    const failing = c.complete(model, { lineNumber: 1, column: 15 }, new FakeToken());
    await flush();
    failNext("insight_complete", { category: "internal", message: "boom" });
    expect(await failing).toEqual({ suggestions: [], incomplete: false });

    target = null;
    tauriCore.invoke.mockClear();
    expect(await c.complete(model, { lineNumber: 1, column: 15 }, new FakeToken())).toEqual({
      suggestions: [],
      incomplete: false,
    });
    expect(tauriCore.invoke).not.toHaveBeenCalled();
  });

  it("keeps item ranges on the cursor's line", async () => {
    const c = controller();
    model.setValue("SELECT 1;\nSELECT * FROM o");
    const result = c.complete(model, { lineNumber: 2, column: 16 }, new FakeToken());
    await flush();
    // A range that does not contain the cursor falls back to an empty one at the cursor.
    answerNext(
      "insight_complete",
      completion(1, [item("orders", "table", 24, 25), item("bad", "table", 0, 3)]),
    );
    const [good, bad] = (await result).suggestions;
    expect(good?.range.replace).toEqual({
      startLineNumber: 2,
      startColumn: 15,
      endLineNumber: 2,
      endColumn: 16,
    });
    expect(bad?.range.insert).toEqual({
      startLineNumber: 2,
      startColumn: 16,
      endLineNumber: 2,
      endColumn: 16,
    });
  });
});

describe("diagnostics", () => {
  const UNKNOWN: DiagnosticItem = {
    range: { start: 14, end: 16 },
    severity: "hint",
    code: "unknown-relation",
    message: "No relation named cu in the cache.",
  };
  const OPEN: DiagnosticItem = {
    range: { start: 0, end: 6 },
    severity: "error",
    code: "unbalanced-parenthesis",
    message: "( is not closed.",
  };

  it("runs once, 400 ms after the last edit, and sets markers", async () => {
    vi.useFakeTimers();
    const c = controller();
    c.scheduleDiagnostics();
    vi.advanceTimersByTime(300);
    c.scheduleDiagnostics();
    vi.advanceTimersByTime(300);
    c.scheduleDiagnostics();
    vi.advanceTimersByTime(399);
    expect(calls("insight_diagnose")).toHaveLength(0);
    vi.advanceTimersByTime(1);
    await flush();
    expect(calls("insight_diagnose")).toHaveLength(1);
    const request = calls("insight_diagnose")[0]?.request as InsightRequest;
    expect(request.text).toBe("SELECT * FROM cu");
    expect(request.seq).toBe(1);
    answerNext("insight_diagnose", diagnostics(1, [OPEN, UNKNOWN]));
    await flush();
    expect(markers.set).toHaveBeenCalledTimes(1);
    const set = markers.set.mock.calls[0]?.[0] as MarkerLike[];
    expect(set).toEqual([
      {
        startLineNumber: 1,
        startColumn: 1,
        endLineNumber: 1,
        endColumn: 7,
        severity: ENUMS.MarkerSeverity.Error,
        message: "( is not closed.",
        code: "unbalanced-parenthesis",
        source: "oxbase-insight",
      },
      expect.objectContaining({
        startColumn: 15,
        endColumn: 17,
        severity: ENUMS.MarkerSeverity.Hint,
        code: "unknown-relation",
      }),
    ]);
  });

  it("clears markers when an answer has none, on errors and without an engine", async () => {
    const c = controller();
    void c.runDiagnostics();
    await flush();
    answerNext("insight_diagnose", diagnostics(1, [OPEN]));
    await flush();
    expect(markers.set).toHaveBeenCalledTimes(1);

    void c.runDiagnostics();
    await flush();
    answerNext("insight_diagnose", diagnostics(2, []));
    await flush();
    expect(markers.clear).toHaveBeenCalledTimes(1);

    void c.runDiagnostics();
    await flush();
    answerNext("insight_diagnose", diagnostics(3, [OPEN]));
    await flush();
    void c.runDiagnostics();
    await flush();
    failNext("insight_diagnose", { category: "internal", message: "boom" });
    await flush();
    expect(markers.clear).toHaveBeenCalledTimes(2);

    void c.runDiagnostics();
    await flush();
    answerNext("insight_diagnose", diagnostics(5, [OPEN]));
    await flush();
    target = null;
    await c.runDiagnostics();
    expect(markers.clear).toHaveBeenCalledTimes(3);
  });

  it("drops a stale answer (older request, or the text changed meanwhile)", async () => {
    const c = controller();
    void c.runDiagnostics();
    void c.runDiagnostics();
    await flush();
    answerNext("insight_diagnose", diagnostics(1, [OPEN]));
    await flush();
    expect(markers.set).not.toHaveBeenCalled();
    model.setValue("SELECT * FROM cus");
    answerNext("insight_diagnose", diagnostics(2, [OPEN]));
    await flush();
    expect(markers.set).not.toHaveBeenCalled();
    expect(c.stats.droppedDiagnostics).toBe(2);
  });

  it("closes the editor in the core on dispose and stops pending runs", async () => {
    vi.useFakeTimers();
    const c = controller("editor-9");
    void c.runDiagnostics();
    await flush();
    answerNext("insight_diagnose", diagnostics(1, [OPEN]));
    await flush();
    c.scheduleDiagnostics();
    c.dispose();
    vi.advanceTimersByTime(1000);
    await flush();
    expect(calls("insight_diagnose")).toHaveLength(1);
    expect(calls("insight_close_editor")).toEqual([{ editorId: "editor-9" }]);
    expect(markers.clear).toHaveBeenCalledTimes(1);
  });
});

describe("hover", () => {
  it("maps the answer's range and Markdown", async () => {
    const c = controller();
    const result = c.hover(model, { lineNumber: 1, column: 16 }, new FakeToken());
    await flush();
    const request = calls("insight_hover")[0]?.request as InsightRequest;
    expect(request.offset).toBe(15);
    answerNext("insight_hover", { range: { start: 14, end: 16 }, markdown: "**table** `cu`" });
    expect(await result).toEqual({
      contents: [{ value: "**table** `cu`" }],
      range: { startLineNumber: 1, startColumn: 15, endLineNumber: 1, endColumn: 17 },
    });
    const none = c.hover(model, { lineNumber: 1, column: 2 }, new FakeToken());
    await flush();
    answerNext("insight_hover", null);
    expect(await none).toBeNull();
  });
});

describe("format", () => {
  it("replaces the answered range as one undoable edit", async () => {
    model.setValue("select a from t;\nselect  b from u");
    const editor = new FakeEditor(model);
    const c = controller();
    const result = c.format(editor);
    await flush();
    expect(calls("format_sql")[0]?.request).toEqual({
      engine: "postgres",
      text: "select a from t;\nselect  b from u",
      range: null,
      options: null,
    });
    answerNext("format_sql", {
      range: { start: 0, end: 33 },
      text: "SELECT a\nFROM t;\n\nSELECT b\nFROM u",
    });
    expect(await result).toEqual({ status: "formatted" });
    expect(model.getValue()).toBe("SELECT a\nFROM t;\n\nSELECT b\nFROM u");
    expect(editor.edits).toHaveLength(1);
    expect(editor.undoStops).toBe(2);
  });

  it("formats the selection's statements and reports failures", async () => {
    model.setValue("select 1;\nselect  2;");
    const editor = new FakeEditor(model);
    editor.select(10, 20);
    const c = controller();
    const result = c.format(editor);
    await flush();
    expect((calls("format_sql")[0]?.request as { range: unknown } | undefined)?.range).toEqual({
      start: 10,
      end: 20,
    });
    answerNext("format_sql", { range: { start: 10, end: 20 }, text: "SELECT 2;" });
    expect(await result).toEqual({ status: "formatted" });
    expect(model.getValue()).toBe("select 1;\nSELECT 2;");

    const same = c.format(editor);
    await flush();
    answerNext("format_sql", { range: { start: 0, end: 9 }, text: "select 1;" });
    expect(await same).toEqual({ status: "unchanged" });

    const failing = c.format(editor);
    await flush();
    failNext("format_sql", { category: "internal", message: "the formatted text would differ" });
    expect(await failing).toEqual({
      status: "failed",
      message: "the formatted text would differ",
    });

    const stale = c.format(editor);
    await flush();
    model.setValue("select 3;");
    answerNext("format_sql", { range: { start: 0, end: 9 }, text: "SELECT 3;" });
    expect(await stale).toEqual({ status: "stale" });
    expect(model.getValue()).toBe("select 3;");
    expect(editor.edits).toHaveLength(1);
  });
});
