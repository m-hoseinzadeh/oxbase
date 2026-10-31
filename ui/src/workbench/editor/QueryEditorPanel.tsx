/**
 * A query editor tab: connection and database pickers, run buttons, the SQL editor, a splitter
 * and the results area. Each tab is one `EditorId` in the core, so it has its own database
 * session (opened by the core on the first run, closed with the tab).
 */
import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import {
  type Engine,
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
import { IDLE_RUN, isActive, runReducer } from "../results/runState";
import { Splitter } from "./Splitter";
import { type EditorCommand, SqlEditor, type SqlEditorHandle, type SqlLanguage } from "./SqlEditor";

/** The Monaco grammar for an engine's SQL. */
const LANGUAGE: Partial<Record<Engine, SqlLanguage>> = {
  postgres: "pgsql",
  mysql: "mysql",
  mariadb: "mysql",
};

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
  const editorRef = useRef<SqlEditorHandle | null>(null);
  const runId = useRef<string | null>(null);
  const runSeq = useRef(0);
  const cancelWanted = useRef(false);
  /** The base session the core opened this editor's session from, if any. */
  const editorSession = useRef<string | null>(null);
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
  const releaseEditor = useCallback(() => {
    if (editorSession.current === null) return;
    editorSession.current = null;
    void queryCloseEditor(editorId).catch(() => {});
  }, [editorId]);

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
      editorSession.current = null;
      await queryCloseEditor(editorId).catch(() => {});
    }
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

  function cancel() {
    const id = runId.current;
    if (id) void queryCancel(id).catch(() => {});
    else cancelWanted.current = true;
  }

  const engine = session?.engine;
  const language: SqlLanguage = (engine && LANGUAGE[engine]) ?? "sql";
  const selectedDatabase = database ?? databases.current ?? "";
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
        <button type="button" className="button" disabled={!active} onClick={cancel}>
          Stop
        </button>
        <span className="query-editor__status" role="status" aria-live="polite">
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
          />
        </div>
        <Splitter value={split} onChange={setSplit} label="Resize editor and results" />
        <div className="query-editor__results">
          <ResultsArea run={run} />
        </div>
      </div>
    </section>
  );
}
