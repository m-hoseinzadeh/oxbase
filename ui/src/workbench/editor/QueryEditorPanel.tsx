/**
 * A query editor tab: connection and database pickers, run buttons, the SQL editor, a splitter
 * and the results area. Each tab is one `EditorId` in the core, so it has its own database
 * session (opened by the core on the first run, closed with the tab).
 *
 * 1.3: a tab can show a file or a saved query (`document`), saved with Ctrl+S / Ctrl+Shift+S
 * (../documents). A tab opened on a saved connection (`profileId`: a restored tab, a linked file)
 * uses that connection's open session, or offers to connect it; a file that never ran anywhere
 * (`pickConnection`) starts with no connection chosen. The connection a file tab runs on is
 * remembered for that file.
 */
import {
  Braces,
  ChevronDown,
  CircleCheck,
  CircleSlash,
  CircleX,
  Database,
  Download,
  Info,
  LoaderCircle,
  type LucideIcon,
  Play,
  Plug,
  Save,
  Square,
  TextCursorInput,
  TextSelect,
  TriangleAlert,
  WandSparkles,
} from "lucide-react";
import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import {
  isConfirmationRequired,
  isTerminalEvent,
  queryCancel,
  queryCloseEditor,
  queryRun,
  statementAt,
  treeChildren,
} from "../../api";
import { ContextMenu, type MenuItem } from "../../components/ContextMenu";
import { ICON_SM } from "../../components/icons";
import { Modal } from "../../components/Modal";
import { Select } from "../../components/Select";
import { toCommandError } from "../../connections/api";
import { requestConnect } from "../../connections/requests";
import { useConnections } from "../../connections/store";
import { ENGINE_INFO } from "../../connections/types";
import { rememberLink, saveTab, saveTabAs, saveTabAsQuery } from "../documents/actions";
import { noteEditorReady, noteVersion, registerQueryTab, useTabDirty } from "../documents/tabs";
import { notifyHistoryChanged } from "../events";
import { formatMs } from "../format";
import type { QueryEditorParams } from "../layout";
import { spanOffsets } from "../positions";
import { ResultsArea } from "../results/ResultsArea";
import { IDLE_RUN, isActive, type ResultInfo, runReducer } from "../results/runState";
import { registerShortcuts, type ShortcutHandler } from "../shortcuts";
import { openExport } from "../transfer/store";
import { hasInsight } from "./insight/controller";
import { SchemaCacheIndicator } from "./insight/SchemaCacheIndicator";
import { SnippetManager } from "./insight/SnippetManager";
import { useSchemaCache } from "./insight/useSchemaCache";
import { type EditorLanguage, languageFor, messagesNoteFor } from "./language";
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

type StatusTone = "idle" | "running" | "finished" | "failed" | "cancelled" | "hint" | "rolledBack";

/** The run status icon in the toolbar (the text says the same; colour is never the only cue). */
const STATUS_ICON: Record<StatusTone, LucideIcon | null> = {
  idle: null,
  running: LoaderCircle,
  finished: CircleCheck,
  failed: CircleX,
  cancelled: CircleSlash,
  hint: Info,
  rolledBack: TriangleAlert,
};

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
  document: initialDocument,
  profileId: initialProfile,
  restoredDirty,
  pickConnection,
}: QueryEditorParams) {
  const { sessions, profiles } = useConnections();
  const [chosenSession, setChosenSession] = useState<string | null>(initialSession ?? null);
  const [database, setDatabase] = useState<string | null>(initialDatabase ?? null);
  const [databases, setDatabases] = useState<DatabaseList>(NO_DATABASES);
  const [run, dispatch] = useReducer(runReducer, IDLE_RUN);
  const [hint, setHint] = useState<string | null>(null);
  const [split, setSplit] = useState(0.45);
  const [snippetsOpen, setSnippetsOpen] = useState(false);
  /** A run the core refused until the user confirms (mongo shell `drop()`, Redis `FLUSHDB`). */
  const [confirm, setConfirm] = useState<{ script: string; message: string } | null>(null);
  const editorRef = useRef<SqlEditorHandle | null>(null);
  const runId = useRef<string | null>(null);
  const runSeq = useRef(0);
  const cancelWanted = useRef(false);
  /** The base session the core opened this editor's session from, if any. */
  const editorSession = useRef<string | null>(null);
  /** A `query_close_editor` still in flight: the next run waits for it (same `EditorId`). */
  const pendingClose = useRef<Promise<void> | null>(null);
  const startedAt = useRef(0);

  /** The saved connection the tab last ran on (kept while it is not connected). */
  const lastProfile = useRef<string | null>(initialProfile ?? null);
  // Without a choice, the editor uses an open session of its saved connection, else (unless it
  // must ask) the first open connection.
  const profileSession = lastProfile.current
    ? sessions.find((s) => s.profile_id === lastProfile.current)
    : undefined;
  const sessionId =
    chosenSession ??
    profileSession?.id ??
    (lastProfile.current || pickConnection ? null : (sessions[0]?.id ?? null));
  const session = sessions.find((s) => s.id === sessionId) ?? null;
  if (session) lastProfile.current = session.profile_id;
  /** A saved connection the tab runs on that is not connected. */
  const waitingProfile =
    !session && chosenSession === null && lastProfile.current
      ? (profiles.find((p) => p.id === lastProfile.current) ?? null)
      : null;
  const [saveMenu, setSaveMenu] = useState<{ x: number; y: number } | null>(null);
  const saveMenuButton = useRef<HTMLButtonElement>(null);
  const dirty = useTabDirty(editorId);
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

  async function execute(script: string, confirmDestructive = false) {
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
          if (isTerminalEvent(event)) {
            notifyHistoryChanged();
            // A schema change the run made bumps the cache generation: read the status again.
            cache.noteRunFinished();
          }
        },
        database || confirmDestructive
          ? {
              ...(database ? { database } : {}),
              ...(confirmDestructive ? { confirmDestructive: true } : {}),
            }
          : undefined,
      );
      if (seq !== runSeq.current) return;
      runId.current = id;
      dispatch({ type: "runId", runId: id });
      if (cancelWanted.current) void queryCancel(id).catch(() => {});
    } catch (error) {
      if (seq !== runSeq.current) return;
      const err = toCommandError(error);
      dispatch({ type: "startFailed", error: err });
      if (isConfirmationRequired(err)) setConfirm({ script, message: err.message });
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

  // App-wide shortcuts (../shortcuts): F5 / Ctrl+R run the selection or the whole script,
  // Ctrl+Enter and Ctrl+Shift+Enter as in the editor, Shift+F5 stops. They read the latest
  // state through the ref, registered once per tab.
  const shortcut = useRef<ShortcutHandler>(() => false);
  shortcut.current = (command) => {
    // 1.3: Ctrl+S saves the tab's file or saved query, Ctrl+Shift+S saves it as a file.
    if (command === "save") {
      void saveTab(editorId);
      return true;
    }
    if (command === "saveAs") {
      void saveTabAs(editorId);
      return true;
    }
    // Ctrl+N adds a table row in a table tab: not a query tab's.
    if (command === "new") return false;
    if (command === "cancel") {
      if (!active) return false;
      cancel();
      return true;
    }
    // A run key while nothing can run (no connection, a query running) is used up, not passed on.
    if (!session || active) return true;
    if (command === "run") {
      if (editorRef.current?.getSelection()) runSelection();
      else runAll();
    } else onCommand(command === "runAll" ? "runAll" : "runCurrent");
    return true;
  };
  useEffect(() => registerShortcuts(editorId, (command) => shortcut.current(command)), [editorId]);

  // 1.3: the document code reads the tab's text, version and connection through this.
  const target = useRef<{ profileId?: string; database?: string; profileName?: string }>({});
  target.current = {
    ...(lastProfile.current ? { profileId: lastProfile.current } : {}),
    ...((database ?? databases.current)
      ? { database: (database ?? databases.current) as string }
      : {}),
    ...(session
      ? { profileName: session.profile_name }
      : waitingProfile
        ? { profileName: waitingProfile.name }
        : {}),
  };
  const opened = useRef({ sql: sql ?? "", document: initialDocument, dirty: restoredDirty });
  useEffect(
    () =>
      registerQueryTab(
        editorId,
        {
          getText: () => editorRef.current?.getText() ?? opened.current.sql,
          getVersion: () => editorRef.current?.getVersion?.() ?? 1,
          target: () => target.current,
        },
        { document: opened.current.document, dirty: opened.current.dirty },
      ),
    [editorId],
  );

  // A file tab remembers the connection and database it runs on (the next open uses them).
  const linkProfile = session?.profile_id ?? null;
  const linkDatabase = database;
  useEffect(() => {
    if (!linkProfile) return;
    rememberLink(editorId, {
      profileId: linkProfile,
      ...(linkDatabase ? { database: linkDatabase } : {}),
    });
  }, [editorId, linkProfile, linkDatabase]);

  function showSaveMenu() {
    const rect = saveMenuButton.current?.getBoundingClientRect();
    setSaveMenu({ x: rect?.left ?? 0, y: rect?.bottom ?? 0 });
  }

  const saveItems: MenuItem[] = [
    { label: "Save", icon: Save, shortcut: "Ctrl+S", onSelect: () => void saveTab(editorId) },
    {
      label: "Save as file…",
      shortcut: "Ctrl+Shift+S",
      onSelect: () => void saveTabAs(editorId),
    },
    {
      label: "Save as saved query…",
      disabled: !lastProfile.current,
      onSelect: () => void saveTabAsQuery(editorId),
    },
  ];

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
  const language: EditorLanguage = languageFor(engine);
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
  let statusTone: StatusTone = "idle";
  if (!session) {
    status = waitingProfile
      ? `Connect to ${waitingProfile.name} to run this query.`
      : sessions.length === 0
        ? "Open a connection to run queries."
        : "Choose a connection.";
  } else if (active) {
    const n = run.current;
    status = `Running${n === null ? "" : ` statement ${n + 1} of ${run.statements.length}`}…`;
    statusTone = "running";
  } else if (run.status === "finished") {
    status = "Finished";
    statusTone = "finished";
  } else if (run.status === "failed") {
    status = "Failed";
    statusTone = "failed";
  } else if (run.status === "cancelled") {
    status = "Cancelled";
    statusTone = "cancelled";
  }
  // Per-run connections: the transaction the run left open was not kept (see Messages).
  if (!active && run.rolledBack) {
    status = `${status} · transaction rolled back`;
    statusTone = "rolledBack";
  }
  if (hint) statusTone = "hint";
  const StatusIcon = STATUS_ICON[statusTone];

  return (
    <section className="query-editor" aria-label="Query editor">
      <div className="toolbar query-editor__toolbar" role="toolbar" aria-label="Query actions">
        <label
          className="query-editor__picker query-editor__picker--connection"
          title="Connection the query runs on"
        >
          <span className="visually-hidden">Connection</span>
          {session ? (
            <span
              className={`engine-badge engine-badge--${session.engine} query-editor__engine`}
              aria-hidden="true"
            >
              {ENGINE_INFO[session.engine].short}
            </span>
          ) : (
            <Plug {...ICON_SM} className="query-editor__picker-icon" />
          )}
          <Select
            aria-label="Connection"
            value={sessionId ?? ""}
            disabled={active || sessions.length === 0}
            onChange={(e) => chooseSession(e.target.value)}
          >
            {sessionId === null ? (
              <option value="">
                {waitingProfile
                  ? `${waitingProfile.name} (not connected)`
                  : pickConnection && sessions.length > 0
                    ? "Choose a connection…"
                    : "No open connection"}
              </option>
            ) : null}
            {sessionId !== null && !session ? (
              <option value={sessionId}>Disconnected</option>
            ) : null}
            {sessions.map((s) => (
              <option key={s.id} value={s.id}>
                {s.profile_name} ({ENGINE_INFO[s.engine].label})
              </option>
            ))}
          </Select>
        </label>
        {waitingProfile ? (
          <button
            type="button"
            className="button button--ghost"
            data-testid="connect-profile"
            title={`Connect to ${waitingProfile.name}`}
            onClick={() => requestConnect(waitingProfile.id)}
          >
            <Plug {...ICON_SM} />
            Connect
          </button>
        ) : null}
        <label className="query-editor__picker" title="Database the query runs in">
          <span className="visually-hidden">Database</span>
          {databases.status === "loading" ? (
            <LoaderCircle {...ICON_SM} className="query-editor__picker-icon spin" />
          ) : (
            <Database {...ICON_SM} className="query-editor__picker-icon" />
          )}
          <Select
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
          </Select>
        </label>
        <span className="toolbar__separator" aria-hidden="true" />
        <button
          type="button"
          className="button button--primary query-editor__run"
          data-testid="run-button"
          disabled={disabled}
          onClick={runAll}
          title="Run the selection, else the whole script (F5, Ctrl+R; the whole script: Ctrl+Shift+Enter)"
        >
          <Play {...ICON_SM} fill="currentColor" />
          Run
        </button>
        <button
          type="button"
          className="button button--ghost"
          disabled={disabled}
          onClick={() => void runCurrent()}
          title="Run the statement at the cursor (Ctrl+Enter)"
        >
          <TextCursorInput {...ICON_SM} />
          Run current
        </button>
        <button
          type="button"
          className="button button--ghost"
          disabled={disabled}
          onClick={runSelection}
          title="Run the selected SQL (Ctrl+Enter with a selection)"
        >
          <TextSelect {...ICON_SM} />
          Run selection
        </button>
        <button
          type="button"
          className={active ? "button button--danger" : "button button--ghost"}
          data-testid="cancel-button"
          disabled={!active}
          onClick={cancel}
          title="Stop the running query (Shift+F5)"
        >
          <Square {...ICON_SM} fill={active ? "currentColor" : "none"} />
          Stop
        </button>
        <span className="toolbar__separator" aria-hidden="true" />
        <span className="split-button">
          <button
            type="button"
            className="button button--ghost split-button__main"
            data-testid="save-button"
            data-dirty={dirty ? "true" : undefined}
            onClick={() => void saveTab(editorId)}
            title={dirty ? "Save the unsaved changes (Ctrl+S)" : "Save (Ctrl+S)"}
          >
            <Save {...ICON_SM} />
            Save
          </button>
          <button
            ref={saveMenuButton}
            type="button"
            className="button button--ghost split-button__more"
            aria-label="More ways to save"
            aria-haspopup="menu"
            aria-expanded={saveMenu !== null}
            onClick={showSaveMenu}
          >
            <ChevronDown {...ICON_SM} />
          </button>
        </span>
        <button
          type="button"
          className="button button--ghost"
          data-testid="format-button"
          disabled={!insightOn}
          onClick={() => void format()}
          title="Format the selection's statements, or all (Shift+Alt+F)"
        >
          <WandSparkles {...ICON_SM} />
          Format
        </button>
        <button
          type="button"
          className="button button--ghost"
          data-testid="snippets-button"
          onClick={() => setSnippetsOpen(true)}
          title="Manage SQL snippets (type a prefix, or Ctrl+Space for completions)"
        >
          <Braces {...ICON_SM} />
          Snippets…
        </button>
        <button
          type="button"
          className="button button--ghost"
          disabled={!session}
          onClick={() => void exportQuery()}
          title="Export every row of the query at the cursor (or the selection)"
        >
          <Download {...ICON_SM} />
          Export…
        </button>
        <span className="query-editor__spacer" />
        <span
          className="query-editor__status"
          role="status"
          aria-live="polite"
          data-testid="run-status"
          data-state={statusTone}
          title={hint ?? status}
        >
          {StatusIcon ? (
            <StatusIcon
              {...ICON_SM}
              className={`query-editor__status-icon${statusTone === "running" ? " spin" : ""}`}
            />
          ) : null}
          <span className="query-editor__status-text">{hint ?? status}</span>
          {elapsed !== null && run.status !== "idle" ? (
            <span className="query-editor__elapsed">{` · ${formatMs(elapsed)}`}</span>
          ) : null}
        </span>
        <span className="toolbar__separator" aria-hidden="true" />
        <SchemaCacheIndicator view={cache} connected={insightOn && session !== null} />
      </div>
      <div className="query-editor__split">
        <div className="query-editor__editor" style={{ flexBasis: `${split * 100}%` }}>
          <SqlEditor
            initialValue={sql ?? ""}
            language={language}
            editorRef={editorRef}
            onReady={(version) => noteEditorReady(editorId, version)}
            onVersionChange={(version) => noteVersion(editorId, version)}
            onCommand={onCommand}
            ariaLabel="SQL editor"
            insight={insight}
          />
        </div>
        <Splitter value={split} onChange={setSplit} label="Resize editor and results" />
        <div className="query-editor__results">
          <ResultsArea
            run={run}
            messagesNote={messagesNoteFor(engine)}
            onExport={exportResult}
            engine={engine ?? null}
          />
        </div>
      </div>
      {confirm ? (
        <Modal
          title="Run anyway?"
          onClose={() => setConfirm(null)}
          footer={
            <>
              <button type="button" className="button" onClick={() => setConfirm(null)}>
                Cancel
              </button>
              <button
                type="button"
                className="button button--danger"
                data-testid="confirm-action"
                data-autofocus
                onClick={() => {
                  const script = confirm.script;
                  setConfirm(null);
                  void execute(script, true);
                }}
              >
                Run anyway
              </button>
            </>
          }
        >
          <p className="confirm__text">
            <TriangleAlert {...ICON_SM} className="confirm__icon" />
            This can change or remove data:{" "}
            {confirm.message.replace(/^confirmation required:\s*/i, "")}
          </p>
        </Modal>
      ) : null}
      {saveMenu ? (
        <ContextMenu
          x={saveMenu.x}
          y={saveMenu.y}
          label="Save"
          items={saveItems}
          onClose={() => setSaveMenu(null)}
        />
      ) : null}
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
