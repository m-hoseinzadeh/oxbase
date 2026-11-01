/**
 * A query editor tab: connection and database pickers, run buttons, the SQL editor, a splitter
 * and the results area. Each tab is one `EditorId` in the core, so it has its own database
 * session (opened by the core on the first run, closed with the tab).
 */
import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import {
  isTerminalEvent,
  queryCancel,
  queryCloseEditor,
  queryRun,
  statementAt,
  treeChildren,
} from "../../api";
import { toCommandError } from "../../connections/api";
import { useConnections } from "../../connections/store";
import { ENGINE_INFO } from "../../connections/types";
import { notifyHistoryChanged } from "../events";
import { formatMs } from "../format";
import type { QueryEditorParams } from "../layout";
import { spanOffsets } from "../positions";
import { ResultsArea } from "../results/ResultsArea";
import { IDLE_RUN, isActive, type ResultInfo, runReducer } from "../results/runState";
import { openExport } from "../transfer/store";
import { hasInsight } from "./insight/controller";
import { SchemaCacheIndicator } from "./insight/SchemaCacheIndicator";
import { SnippetManager } from "./insight/SnippetManager";
import { useSchemaCache } from "./insight/useSchemaCache";
import { languageFor, messagesNoteFor, type SqlLanguage } from "./language";
import { Splitter } from "./Splitter";
import {
  type EditorCommand,
  type EditorInsight,
  type FormatOutcome,
  SqlEditor,
  type SqlEditorHandle,
} from "./SqlEditor";

interface DatabaseList {
  status: "idle" | "loading" | "ok" | "error";
  names: string[];
  /** The session's own database (the `current` listing extra). */
  current: string | null;
}

const NO_DATABASES: DatabaseList = { status: "idle", names: [], current: null };

/** Ticks while `active`, so the running time stays current. */
function useNow(active: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    setNow(Date.now());
    const timer = setInterval(() => setNow(Date.now()), 250);
    return () => clearInterval(timer);
  }, [active]);
  return now;
}

export function QueryEditorPanel({
  editorId,
  sessionId: initialSession,
  database: initialDatabase,
  sql,
}: QueryEditorParams) {
  const { sessions } = useConnections();
  const [chosenSession, setChosenSession] = useState<string | null>(initialSession ?? null);
  const [database, setDatabase] = useState<string | null>(initialDatabase ?? null);
  const [databases, setDatabases] = useState<DatabaseList>(NO_DATABASES);
  const [run, dispatch] = useReducer(runReducer, IDLE_RUN);
  const [hint, setHint] = useState<string | null>(null);
  const [split, setSplit] = useState(0.45);
  const [snippetsOpen, setSnippetsOpen] = useState(false);
  const editorRef = useRef<SqlEditorHandle | null>(null);
  const runId = useRef<string | null>(null);
  const runSeq = useRef(0);
  const cancelWanted = useRef(false);
  /** The base session the core opened this editor's session from, if any. */
  const editorSession = useRef<string | null>(null);
  /** A `query_close_editor` still in flight: the next run waits for it (same `EditorId`). */
  const pendingClose = useRef<Promise<void> | null>(null);
  const startedAt = useRef(0);

  // Without a choice, the editor uses the first open connection.
  const sessionId = chosenSession ?? sessions[0]?.id ?? null;
  const session = sessions.find((s) => s.id === sessionId) ?? null;
  const active = isActive(run.status);
  const now = useNow(active);

  // The databases of the connection, for the picker.
  const sessionKey = session?.id ?? null;
  useEffect(() => {
    if (sessionKey === null) {
      setDatabases(NO_DATABASES);
      return;
    }
    let alive = true;
    setDatabases({ ...NO_DATABASES, status: "loading" });
    treeChildren(sessionKey, { type: "connection" }).then(
      (nodes) => {
        if (!alive) return;
        const names: string[] = [];
        let current: string | null = null;
        for (const node of nodes) {
          if (node.node.type !== "object" || node.node.object.kind !== "database") continue;
          names.push(node.node.object.name);
          const flag = node.summary?.extras?.current;
          if (flag?.type === "bool" && flag.value) current = node.node.object.name;
        }
        setDatabases({ status: "ok", names, current });
      },
      () => {
        if (alive) setDatabases({ ...NO_DATABASES, status: "error" });
      },
    );
    return () => {
      alive = false;
    };
  }, [sessionKey]);

  // A tab that switches connection gives up its session on the old one.
  const closeEditorSession = useCallback((): Promise<void> => {
    editorSession.current = null;
    const closing = queryCloseEditor(editorId)
      .catch(() => {})
      .finally(() => {
        if (pendingClose.current === closing) pendingClose.current = null;
      });
    pendingClose.current = closing;
    return closing;
  }, [editorId]);

  const releaseEditor = useCallback(() => {
    if (editorSession.current === null) return;
    void closeEditorSession();
  }, [closeEditorSession]);

  useEffect(() => releaseEditor, [releaseEditor]);

  function chooseSession(id: string) {
    if (id === sessionId) return;
    releaseEditor();
    runSeq.current += 1;
    dispatch({ type: "reset" });
    setChosenSession(id);
    setDatabase(null);
  }

  async function execute(script: string) {
    if (!session || active) return;
    if (!script.trim()) {
      setHint("Nothing to run.");
      return;
    }
    setHint(null);
    runSeq.current += 1;
    const seq = runSeq.current;
    runId.current = null;
    cancelWanted.current = false;
    startedAt.current = Date.now();
    dispatch({ type: "start", script });
    // The editor's core session belongs to one connection: moving to another (the first open
    // connection closed, say) closes it first, and the tab stays on the new one.
    if (editorSession.current !== null && editorSession.current !== session.id) {
      await closeEditorSession();
    }
    // A close sent when the user switched connection must land before this run opens the
    // editor's new session, or it could close that one.
    while (pendingClose.current) await pendingClose.current;
    if (seq !== runSeq.current) return;
    setChosenSession(session.id);
    editorSession.current = session.id;
    try {
      const id = await queryRun(
        session.id,
        editorId,
        script,
        (event) => {
          if (seq !== runSeq.current) return;
          dispatch({ type: "event", event });
          if (isTerminalEvent(event)) notifyHistoryChanged();
        },
        database ? { database } : undefined,
      );
      if (seq !== runSeq.current) return;
      runId.current = id;
      dispatch({ type: "runId", runId: id });
      if (cancelWanted.current) void queryCancel(id).catch(() => {});
    } catch (error) {
      if (seq === runSeq.current) dispatch({ type: "startFailed", error: toCommandError(error) });
    }
  }

  function runAll() {
    const editor = editorRef.current;
    if (editor) void execute(editor.getText());
  }

  function runSelection() {
    const selection = editorRef.current?.getSelection();
    if (!selection) {
      setHint("Select the SQL to run first.");
      return;
    }
    void execute(selection.text);
  }

  async function runCurrent() {
    const editor = editorRef.current;
    if (!editor || !session) return;
    const text = editor.getText();
    try {
      const span = await statementAt(session.engine, text, editor.getCursorOffset());
      if (!span) {
        setHint("No statement at the cursor.");
        return;
      }
      const [start, end] = spanOffsets(text, span);
      editor.select(start, end);
      await execute(text.slice(start, end));
    } catch (error) {
      setHint(toCommandError(error).message);
    }
  }

  function onCommand(command: EditorCommand) {
    if (command === "runAll") runAll();
    else if (editorRef.current?.getSelection()) runSelection();
    else void runCurrent();
  }

  /** Exports the query at hand: the selection, else the statement at the cursor, else all. */
  async function exportQuery() {
    const editor = editorRef.current;
    if (!editor || !session) return;
    const text = editor.getText();
    let sql = editor.getSelection()?.text ?? "";
    if (!sql.trim()) {
      try {
        const span = await statementAt(session.engine, text, editor.getCursorOffset());
        if (span) {
          const [start, end] = spanOffsets(text, span);
          sql = text.slice(start, end);
        }
      } catch {
        // The whole text is exported; the core refuses more than one statement.
      }
    }
    if (!sql.trim()) sql = text;
    if (!sql.trim()) {
      setHint("Nothing to export.");
      return;
    }
    setHint(null);
    const db = database ?? databases.current;
    openExport({
      source: { type: "query", session: session.id, sql, ...(db ? { database: db } : {}) },
      engine: session.engine,
      label: "the query's rows",
      baseName: "query",
    });
  }

  function exportResult(result: ResultInfo) {
    if (!run.runId) return;
    openExport({
      source: { type: "result", runId: run.runId, resultIndex: result.resultIndex },
      ...(session ? { engine: session.engine } : {}),
      label: `Result ${result.resultIndex + 1}`,
      baseName: `result-${result.resultIndex + 1}`,
      totalRows: result.rowCount,
    });
  }

  function cancel() {
    const id = runId.current;
    if (id) void queryCancel(id).catch(() => {});
    else cancelWanted.current = true;
  }

  const engine = session?.engine;
  const language: SqlLanguage = languageFor(engine);
  const selectedDatabase = database ?? databases.current ?? "";
  const insightOn = hasInsight(engine);
  // The cache of the editor's connection and database (the session's own one when unset).
  const insightDatabase = database ?? databases.current ?? null;
  const cache = useSchemaCache(insightOn && session ? session.id : null, insightDatabase);

  function onFormat(outcome: FormatOutcome) {
    switch (outcome.status) {
      case "failed":
        setHint(`Could not format: ${outcome.message}`);
        break;
      case "stale":
        setHint("The text changed while formatting; nothing was replaced.");
        break;
      case "unavailable":
        setHint("Formatting needs a connection to a SQL database.");
        break;
      case "unchanged":
        setHint("Already formatted.");
        break;
      case "formatted":
        setHint(null);
        break;
    }
  }

  async function format() {
    const editor = editorRef.current;
    if (!editor?.format) return;
    // The editor reports the outcome through `onFormat`.
    await editor.format();
    editor.focus();
  }

  const insight: EditorInsight = {
    editorId,
    engine: insightOn ? (engine ?? null) : null,
    sessionId: session?.id ?? null,
    database: insightDatabase,
    generation: cache.status?.generation ?? null,
    onCacheState: cache.noteCacheState,
    onFormat,
  };
  const elapsed = active ? now - startedAt.current : run.elapsedMs;
  const disabled = !session || active;

  let status = "";
  if (!session) {
    status = sessions.length === 0 ? "Open a connection to run queries." : "Choose a connection.";
  } else if (active) {
    const n = run.current;
    status = `Running${n === null ? "" : ` statement ${n + 1} of ${run.statements.length}`}…`;
  } else if (run.status === "finished") status = "Finished";
  else if (run.status === "failed") status = "Failed";
  else if (run.status === "cancelled") status = "Cancelled";

  return (
    <section className="query-editor" aria-label="Query editor">
      <div className="toolbar query-editor__toolbar" role="toolbar" aria-label="Query actions">
        <label className="query-editor__picker">
          <span className="visually-hidden">Connection</span>
          <select
            aria-label="Connection"
            value={sessionId ?? ""}
            disabled={active || sessions.length === 0}
            onChange={(e) => chooseSession(e.target.value)}
          >
            {sessionId === null ? <option value="">No open connection</option> : null}
            {sessionId !== null && !session ? (
              <option value={sessionId}>Disconnected</option>
            ) : null}
            {sessions.map((s) => (
              <option key={s.id} value={s.id}>
                {s.profile_name} ({ENGINE_INFO[s.engine].label})
              </option>
            ))}
          </select>
        </label>
        <label className="query-editor__picker">
          <span className="visually-hidden">Database</span>
          <select
            aria-label="Database"
            value={selectedDatabase}
            disabled={active || databases.names.length === 0}
            onChange={(e) => setDatabase(e.target.value || null)}
          >
            {databases.names.length === 0 ? (
              <option value="">
                {databases.status === "loading" ? "Loading…" : "Default database"}
              </option>
            ) : null}
            {selectedDatabase !== "" && !databases.names.includes(selectedDatabase) ? (
              <option value={selectedDatabase}>{selectedDatabase}</option>
            ) : null}
            {databases.names.map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
        </label>
        <span className="toolbar__divider" aria-hidden="true" />
        <button
          type="button"
          className="button button--primary"
          data-testid="run-button"
          disabled={disabled}
          onClick={runAll}
          title="Run all (Ctrl+Shift+Enter)"
        >
          Run
        </button>
        <button type="button" className="button" disabled={disabled} onClick={runSelection}>
          Run selection
        </button>
        <button
          type="button"
          className="button"
          disabled={disabled}
          onClick={() => void runCurrent()}
          title="Run the statement at the cursor (Ctrl+Enter)"
        >
          Run current
        </button>
        <button
          type="button"
          className="button"
          data-testid="cancel-button"
          disabled={!active}
          onClick={cancel}
        >
          Stop
        </button>
        <span className="toolbar__divider" aria-hidden="true" />
        <button
          type="button"
          className="button"
          disabled={!session}
          onClick={() => void exportQuery()}
          title="Export every row of the query at the cursor (or the selection)"
        >
          Export…
        </button>
        <span className="toolbar__divider" aria-hidden="true" />
        <button
          type="button"
          className="button"
          data-testid="format-button"
          disabled={!insightOn}
          onClick={() => void format()}
          title="Format the selection's statements, or all (Shift+Alt+F)"
        >
          Format
        </button>
        <button
          type="button"
          className="button"
          data-testid="snippets-button"
          onClick={() => setSnippetsOpen(true)}
          title="Manage SQL snippets"
        >
          Snippets…
        </button>
        <SchemaCacheIndicator view={cache} connected={insightOn && session !== null} />
        <span
          className="query-editor__status"
          role="status"
          aria-live="polite"
          data-testid="run-status"
        >
          {hint ?? status}
          {elapsed !== null && run.status !== "idle" ? ` · ${formatMs(elapsed)}` : ""}
        </span>
      </div>
      <div className="query-editor__split">
        <div className="query-editor__editor" style={{ flexBasis: `${split * 100}%` }}>
          <SqlEditor
            initialValue={sql ?? ""}
            language={language}
            editorRef={editorRef}
            onCommand={onCommand}
            ariaLabel="SQL editor"
            insight={insight}
          />
        </div>
        <Splitter value={split} onChange={setSplit} label="Resize editor and results" />
        <div className="query-editor__results">
          <ResultsArea run={run} messagesNote={messagesNoteFor(engine)} onExport={exportResult} />
        </div>
      </div>
      {snippetsOpen ? (
        <SnippetManager
          engine={insightOn ? (engine ?? null) : null}
          onClose={() => setSnippetsOpen(false)}
          onInsert={(body) => editorRef.current?.insertSnippet?.(body)}
        />
      ) : null}
    </section>
  );
}
