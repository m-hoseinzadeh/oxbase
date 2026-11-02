/**
 * The Redis console (docs/m7-contracts.md §4.3): one command per line, run with `query_run` on
 * the tab's own editor session (so `SELECT`, `MULTI` ... `EXEC` typed line by line work), its
 * replies shown `redis-cli`-like with their RESP types. Up / Down walk the command history;
 * commands the console guards (`FLUSHDB`, `KEYS`, ...) ask before they run again with
 * `confirmDestructive`; blocked ones (`SUBSCRIBE`: use the Pub/Sub tab) say why.
 */
import { CircleSlash, Eraser, LoaderCircle, Play, Square, TriangleAlert } from "lucide-react";
import { type KeyboardEvent, useEffect, useRef, useState } from "react";
import {
  type ColumnInfo,
  type ErrorInfo,
  isConfirmationRequired,
  isTerminalEvent,
  queryCancel,
  queryCloseEditor,
  queryPage,
  queryRun,
  type RunEvent,
  type StatementSpan,
} from "../../../api";
import { ICON, ICON_SM } from "../../../components/icons";
import { Modal } from "../../../components/Modal";
import { toCommandError } from "../../../connections/api";
import { useConnections } from "../../../connections/store";
import { ENGINE_INFO } from "../../../connections/types";
import { notifyHistoryChanged } from "../../events";
import type { ConsolePanelParams } from "../../layout";
import { spanText } from "../../positions";
import { type RespOutput, respOutput } from "./resp";

/** Rows of a reply the console shows (the rest is counted). */
export const CONSOLE_ROWS = 500;
/** Entries kept in the transcript. */
export const MAX_ENTRIES = 200;
/** Commands kept in the history. */
export const MAX_HISTORY = 500;

interface StatementOutput {
  index: number;
  text: string;
  outputs: {
    resultIndex: number;
    columns: ColumnInfo[];
    total: number;
    shown: RespOutput | null;
  }[];
  rowsAffected?: number;
  error?: ErrorInfo;
  notices: string[];
  done: boolean;
}

interface Entry {
  id: number;
  script: string;
  status: "running" | "finished" | "failed" | "cancelled";
  statements: StatementOutput[];
  error?: ErrorInfo;
  runId?: string;
}

let entryIds = 0;

/** Folds one run event into a transcript entry. */
export function foldEvent(entry: Entry, event: RunEvent, spans: StatementSpan[]): Entry {
  const statement = (index: number, patch: (s: StatementOutput) => StatementOutput): Entry => {
    const existing = entry.statements.find((s) => s.index === index);
    const base: StatementOutput = existing ?? {
      index,
      text: spans[index] ? spanText(entry.script, spans[index]) : "",
      outputs: [],
      notices: [],
      done: false,
    };
    const next = patch(base);
    return {
      ...entry,
      statements: existing
        ? entry.statements.map((s) => (s.index === index ? next : s))
        : [...entry.statements, next].sort((a, b) => a.index - b.index),
    };
  };
  switch (event.type) {
    case "started":
      return { ...entry, runId: event.runId };
    case "statementStarted":
      return statement(event.statementIndex, (s) => s);
    case "resultSet":
      return statement(event.statementIndex, (s) => ({
        ...s,
        outputs: [
          ...s.outputs,
          { resultIndex: event.resultIndex, columns: event.columns, total: 0, shown: null },
        ],
      }));
    case "resultDone":
      return {
        ...entry,
        statements: entry.statements.map((s) => ({
          ...s,
          outputs: s.outputs.map((o) =>
            o.resultIndex === event.resultIndex ? { ...o, total: event.totalRows } : o,
          ),
        })),
      };
    case "notice":
      return event.statementIndex === undefined
        ? entry
        : statement(event.statementIndex, (s) => ({
            ...s,
            notices: [...s.notices, event.message],
          }));
    case "statementDone":
      return statement(event.statementIndex, (s) => ({
        ...s,
        done: true,
        ...(event.rowsAffected === undefined ? {} : { rowsAffected: event.rowsAffected }),
      }));
    case "statementFailed":
      return statement(event.statementIndex, (s) => ({ ...s, done: true, error: event.error }));
    case "finished":
      return { ...entry, status: "finished" };
    case "failed":
      return { ...entry, status: "failed", error: event.error };
    case "cancelled":
      return { ...entry, status: "cancelled" };
    default:
      return entry;
  }
}

export function ConsolePanel({
  editorId,
  sessionId,
  database: initialDatabase,
}: ConsolePanelParams) {
  const session = useConnections().sessions.find((s) => s.id === sessionId);
  const [database] = useState(initialDatabase ?? session?.database ?? "0");
  const [entries, setEntries] = useState<Entry[]>([]);
  const [input, setInput] = useState("");
  const [history, setHistory] = useState<string[]>([]);
  const [historyAt, setHistoryAt] = useState<number | null>(null);
  const [confirm, setConfirm] = useState<{ script: string; message: string } | null>(null);
  const running = entries.some((e) => e.status === "running");
  const log = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const spans = useRef(new Map<number, StatementSpan[]>());
  const runIds = useRef(new Map<number, string>());

  useEffect(
    () => () => {
      void queryCloseEditor(editorId).catch(() => {});
    },
    [editorId],
  );

  // Keep the newest output in view.
  // biome-ignore lint/correctness/useExhaustiveDependencies: scroll after every transcript change.
  useEffect(() => {
    const el = log.current;
    if (el && typeof el.scrollTo === "function") el.scrollTo({ top: el.scrollHeight });
    else if (el) el.scrollTop = el.scrollHeight;
  }, [entries]);

  function update(id: number, fn: (e: Entry) => Entry) {
    setEntries((list) => list.map((e) => (e.id === id ? fn(e) : e)));
  }

  /** Reads the first rows of each result of a finished entry. */
  async function loadOutputs(id: number, runId: string, entry: Entry) {
    for (const s of entry.statements) {
      for (const o of s.outputs) {
        try {
          const table = await queryPage(
            runId,
            o.resultIndex,
            0,
            Math.min(Math.max(o.total, 1), CONSOLE_ROWS),
          );
          const shown = respOutput(o.columns, table, o.total);
          update(id, (e) => ({
            ...e,
            statements: e.statements.map((x) => ({
              ...x,
              outputs: x.outputs.map((y) =>
                y.resultIndex === o.resultIndex ? { ...y, shown } : y,
              ),
            })),
          }));
        } catch (error) {
          const message = toCommandError(error).message;
          update(id, (e) => ({
            ...e,
            statements: e.statements.map((x) =>
              x.index === s.index
                ? { ...x, notices: [...x.notices, `could not read the reply: ${message}`] }
                : x,
            ),
          }));
        }
      }
    }
  }

  async function execute(script: string, confirmDestructive = false) {
    if (!script.trim() || running) return;
    entryIds += 1;
    const id = entryIds;
    const entry: Entry = { id, script, status: "running", statements: [] };
    setEntries((list) => [...list, entry].slice(-MAX_ENTRIES));
    let current = entry;
    try {
      const runId = await queryRun(
        sessionId,
        editorId,
        script,
        (event) => {
          if (event.type === "started") spans.current.set(id, event.statements);
          current = foldEvent(current, event, spans.current.get(id) ?? []);
          const snapshot = current;
          update(id, () => snapshot);
          if (isTerminalEvent(event)) {
            notifyHistoryChanged();
            const rid = runIds.current.get(id) ?? snapshot.runId;
            if (rid) void loadOutputs(id, rid, snapshot);
          }
        },
        { database, ...(confirmDestructive ? { confirmDestructive: true } : {}) },
      );
      runIds.current.set(id, runId);
      update(id, (e) => ({ ...e, runId }));
    } catch (error) {
      const err = toCommandError(error);
      if (isConfirmationRequired(err)) {
        setEntries((list) => list.filter((e) => e.id !== id));
        setConfirm({ script, message: err.message });
        return;
      }
      update(id, (e) => ({ ...e, status: "failed", error: err }));
    }
  }

  function submit() {
    const script = input;
    if (!script.trim()) return;
    setHistory((h) => [...h.filter((x) => x !== script), script].slice(-MAX_HISTORY));
    setHistoryAt(null);
    setInput("");
    void execute(script);
  }

  function stop() {
    for (const e of entries) {
      const runId = e.status === "running" ? (e.runId ?? runIds.current.get(e.id)) : undefined;
      if (runId) void queryCancel(runId).catch(() => {});
    }
  }

  function onKeyDown(event: KeyboardEvent<HTMLTextAreaElement>) {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      submit();
      return;
    }
    const el = event.currentTarget;
    const firstLine = !el.value.slice(0, el.selectionStart).includes("\n");
    const lastLine = !el.value.slice(el.selectionEnd).includes("\n");
    if (event.key === "ArrowUp" && firstLine && history.length > 0) {
      event.preventDefault();
      const at = historyAt === null ? history.length - 1 : Math.max(historyAt - 1, 0);
      setHistoryAt(at);
      setInput(history[at] ?? "");
    } else if (event.key === "ArrowDown" && lastLine && historyAt !== null) {
      event.preventDefault();
      const at = historyAt + 1;
      if (at >= history.length) {
        setHistoryAt(null);
        setInput("");
      } else {
        setHistoryAt(at);
        setInput(history[at] ?? "");
      }
    } else if (
      event.key === "c" &&
      event.ctrlKey &&
      running &&
      el.selectionStart === el.selectionEnd
    ) {
      event.preventDefault();
      stop();
    }
  }

  const prompt = `db${database}>`;
  return (
    <section className="console" aria-label="Redis console" data-testid="redis-console">
      <div className="toolbar table-data__toolbar" role="toolbar" aria-label="Console actions">
        <span className="console__target">
          {session ? (
            <span
              className={`engine-badge engine-badge--sm engine-badge--${session.engine}`}
              aria-hidden="true"
            >
              {ENGINE_INFO[session.engine].short}
            </span>
          ) : null}
          {session?.profile_name ?? "Disconnected"} · database {database}
        </span>
        <span className="toolbar__separator" aria-hidden="true" />
        <button
          type="button"
          className="button button--ghost"
          disabled={!running}
          onClick={stop}
          title="Stop the running command (Ctrl+C)"
        >
          <Square {...ICON_SM} />
          Stop
        </button>
        <button
          type="button"
          className="button button--ghost"
          disabled={entries.length === 0 || running}
          onClick={() => setEntries([])}
          title="Clear the output (the history stays)"
        >
          <Eraser {...ICON_SM} />
          Clear
        </button>
        <span
          className="query-editor__status table-data__status"
          role="status"
          data-testid="console-status"
        >
          {running ? "Running…" : `${history.length} in history`}
        </span>
      </div>
      <div
        ref={log}
        className="console__log"
        role="log"
        aria-label="Console output"
        data-testid="console-output"
      >
        {entries.length === 0 ? (
          <p className="console__hint">
            Type a command and press Enter (Shift+Enter for a new line, one command per line). Up
            and Down go through the history. Use the Pub/Sub tab to subscribe.
          </p>
        ) : null}
        {entries.map((entry) => (
          <div key={entry.id} className="console__entry" data-status={entry.status}>
            {entry.statements.length === 0 ? (
              <div className="console__command">
                <span className="console__prompt">{prompt}</span> {entry.script}
              </div>
            ) : null}
            {entry.statements.map((s) => (
              <div key={s.index} className="console__statement">
                <div className="console__command">
                  <span className="console__prompt">{prompt}</span> {s.text}
                </div>
                {s.notices.map((n) => (
                  <div key={n} className="console__notice">
                    {n}
                  </div>
                ))}
                {s.outputs.map((o) => (
                  <ReplyView key={o.resultIndex} output={o.shown} total={o.total} />
                ))}
                {s.outputs.length === 0 && s.rowsAffected !== undefined ? (
                  <div className="resp resp--integer">(integer) {s.rowsAffected}</div>
                ) : null}
                {s.outputs.length === 0 && s.rowsAffected === undefined && s.done && !s.error ? (
                  <div className="resp resp--status">OK</div>
                ) : null}
                {s.error ? (
                  <div className="resp resp--error" role="alert">
                    (error) {s.error.message}
                  </div>
                ) : null}
              </div>
            ))}
            {entry.status === "running" ? (
              <div className="console__running">
                <LoaderCircle {...ICON_SM} className="spin" /> running…
              </div>
            ) : null}
            {entry.status === "cancelled" ? (
              <div className="console__notice">
                <CircleSlash {...ICON_SM} /> cancelled
              </div>
            ) : null}
            {entry.status === "failed" && entry.error && entry.statements.every((s) => !s.error) ? (
              <div className="resp resp--error" role="alert">
                (error) {entry.error.message}
              </div>
            ) : null}
          </div>
        ))}
      </div>
      <div className="console__input">
        <label htmlFor={`${editorId}-input`} className="console__prompt">
          {prompt}
        </label>
        <textarea
          id={`${editorId}-input`}
          ref={inputRef}
          className="console__textarea"
          data-testid="console-input"
          aria-label="Command"
          rows={Math.min(6, Math.max(1, input.split("\n").length))}
          spellCheck={false}
          autoComplete="off"
          placeholder="HGETALL user:1"
          value={input}
          onChange={(e) => {
            setInput(e.target.value);
            setHistoryAt(null);
          }}
          onKeyDown={onKeyDown}
        />
        <button
          type="button"
          className="button button--primary"
          data-testid="console-run"
          disabled={running || !input.trim() || !session}
          onClick={submit}
          title="Run (Enter)"
        >
          <Play {...ICON_SM} fill="currentColor" />
          Run
        </button>
      </div>
      {confirm ? (
        <Modal
          title="Run this command?"
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
            <TriangleAlert {...ICON} className="confirm__icon" />
            {confirmationText(confirm.message)}
          </p>
        </Modal>
      ) : null}
    </section>
  );
}

/** The reason of a confirmation error, without its machine prefix. */
export function confirmationText(message: string): string {
  const reason = message.replace(/^confirmation required:\s*/i, "");
  return `This can change or remove data: ${reason}.`.replace(/\.\.$/, ".");
}

function ReplyView({ output, total }: { output: RespOutput | null; total: number }) {
  if (!output) {
    return (
      <div className="console__running">
        <LoaderCircle {...ICON_SM} className="spin" /> reading the reply…
      </div>
    );
  }
  const more = total > CONSOLE_ROWS ? total - CONSOLE_ROWS : 0;
  return (
    <>
      {output.kind === "lines" ? (
        <div className="resp-lines" data-testid="resp-lines">
          {output.lines.map((line, i) => (
            // biome-ignore lint/suspicious/noArrayIndexKey: reply lines are positions.
            <div key={i} className={`resp resp--${line.tone}`}>
              {line.prefix ? <span className="resp__prefix">{line.prefix}</span> : null}
              {line.text}
            </div>
          ))}
        </div>
      ) : (
        <table className="resp-table" data-testid="resp-table">
          <thead>
            <tr>
              {output.columns.map((c) => (
                <th key={c} scope="col">
                  {c}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {output.rows.map((row, r) => (
              // biome-ignore lint/suspicious/noArrayIndexKey: reply rows are positions.
              <tr key={r}>
                {row.map((cell, c) => (
                  // biome-ignore lint/suspicious/noArrayIndexKey: cells are positions.
                  <td key={c}>{cell}</td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {more > 0 ? (
        <div className="console__notice">
          … {more.toLocaleString("en-US")} more rows (open the key in the key browser to page
          through them)
        </div>
      ) : null}
    </>
  );
}
