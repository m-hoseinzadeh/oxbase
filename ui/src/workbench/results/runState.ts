/**
 * What a query editor shows about its current run, folded from the run's `RunEvent`s
 * (docs/m2-contracts.md §5): one entry per result set and the messages list (notices, rows
 * affected, timings, errors). Events carry counts and column metadata only; rows are paged
 * separately.
 */
import type { ColumnInfo, ErrorInfo, NoticeLevel, RunEvent, StatementSpan } from "../../api";
import { spanText, sqlPreview } from "../positions";

export type RunStatus = "idle" | "starting" | "running" | "finished" | "failed" | "cancelled";

export interface ResultInfo {
  resultIndex: number;
  statementIndex: number;
  statement: StatementSpan;
  columns: ColumnInfo[];
  /** Rows stored so far (grows while streaming). */
  rowCount: number;
  done: boolean;
  truncated: boolean;
}

export type MessageKind = "notice" | "ok" | "error" | "summary";

export interface RunMessage {
  id: number;
  kind: MessageKind;
  level?: NoticeLevel;
  statementIndex?: number;
  /** A one-line preview of the statement the message is about. */
  statement?: string;
  text: string;
  elapsedMs?: number;
}

export interface RunState {
  runId: string | null;
  status: RunStatus;
  /** The script that was sent (statement spans index into it). */
  script: string;
  statements: StatementSpan[];
  /** The statement running now. */
  current: number | null;
  results: ResultInfo[];
  messages: RunMessage[];
  error: ErrorInfo | null;
  elapsedMs: number | null;
}

export type RunAction =
  | { type: "start"; script: string }
  | { type: "runId"; runId: string }
  | { type: "event"; event: RunEvent }
  /** The run could not start (the command rejected). */
  | { type: "startFailed"; error: ErrorInfo }
  | { type: "reset" };

export const IDLE_RUN: RunState = {
  runId: null,
  status: "idle",
  script: "",
  statements: [],
  current: null,
  results: [],
  messages: [],
  error: null,
  elapsedMs: null,
};

export function isActive(status: RunStatus): boolean {
  return status === "starting" || status === "running";
}

function plural(n: number, one: string, many: string): string {
  return `${n.toLocaleString("en-US")} ${n === 1 ? one : many}`;
}

function addMessage(state: RunState, message: Omit<RunMessage, "id">): RunMessage[] {
  return [...state.messages, { ...message, id: state.messages.length }];
}

function statementPreview(state: RunState, index: number): string | undefined {
  const span = state.statements[index];
  return span ? sqlPreview(spanText(state.script, span), 160) : undefined;
}

function updateResult(
  state: RunState,
  resultIndex: number,
  patch: (r: ResultInfo) => ResultInfo,
): ResultInfo[] {
  return state.results.map((r) => (r.resultIndex === resultIndex ? patch(r) : r));
}

function applyEvent(state: RunState, event: RunEvent): RunState {
  switch (event.type) {
    case "started":
      return {
        ...state,
        runId: event.runId,
        status: "running",
        statements: event.statements,
      };
    case "statementStarted":
      return { ...state, current: event.statementIndex };
    case "resultSet":
      return {
        ...state,
        results: [
          ...state.results,
          {
            resultIndex: event.resultIndex,
            statementIndex: event.statementIndex,
            statement: event.statementRange,
            columns: event.columns,
            rowCount: 0,
            done: false,
            truncated: false,
          },
        ],
      };
    case "rows":
      return {
        ...state,
        results: updateResult(state, event.resultIndex, (r) =>
          event.totalSoFar > r.rowCount ? { ...r, rowCount: event.totalSoFar } : r,
        ),
      };
    case "resultDone":
      return {
        ...state,
        results: updateResult(state, event.resultIndex, (r) => ({
          ...r,
          rowCount: event.totalRows,
          done: true,
          truncated: event.truncated,
        })),
      };
    case "notice":
      return {
        ...state,
        messages: addMessage(state, {
          kind: "notice",
          level: event.level,
          ...(event.statementIndex === undefined ? {} : { statementIndex: event.statementIndex }),
          text: event.code ? `${event.message} (${event.code})` : event.message,
        }),
      };
    case "statementDone": {
      const rows = state.results
        .filter((r) => r.statementIndex === event.statementIndex)
        .reduce((sum, r) => sum + r.rowCount, 0);
      const hasResult = state.results.some((r) => r.statementIndex === event.statementIndex);
      const parts: string[] = [];
      if (hasResult) parts.push(plural(rows, "row returned", "rows returned"));
      if (event.rowsAffected !== undefined) {
        parts.push(plural(event.rowsAffected, "row affected", "rows affected"));
      }
      if (parts.length === 0) parts.push("OK");
      const preview = statementPreview(state, event.statementIndex);
      return {
        ...state,
        current: null,
        messages: addMessage(state, {
          kind: "ok",
          statementIndex: event.statementIndex,
          ...(preview ? { statement: preview } : {}),
          text: parts.join(", "),
          elapsedMs: event.elapsedMs,
        }),
      };
    }
    case "statementFailed": {
      const preview = statementPreview(state, event.statementIndex);
      return {
        ...state,
        current: null,
        messages: addMessage(state, {
          kind: "error",
          statementIndex: event.statementIndex,
          ...(preview ? { statement: preview } : {}),
          text: event.error.message,
          elapsedMs: event.elapsedMs,
        }),
      };
    }
    case "finished":
    case "failed":
    case "cancelled": {
      const status = event.type;
      const error = event.type === "failed" ? event.error : null;
      const text =
        status === "finished"
          ? `Finished: ${plural(state.statements.length, "statement", "statements")}`
          : status === "cancelled"
            ? "Cancelled"
            : `Failed: ${error?.message ?? "unknown error"}`;
      // A cancelled or failed run leaves its open results incomplete.
      const results = state.results.map((r) =>
        r.done ? r : { ...r, done: true, truncated: status !== "finished" || r.truncated },
      );
      return {
        ...state,
        status,
        current: null,
        error,
        results,
        elapsedMs: event.elapsedMs,
        messages: addMessage(state, {
          kind: status === "finished" ? "summary" : "error",
          text,
          elapsedMs: event.elapsedMs,
        }),
      };
    }
  }
}

export function runReducer(state: RunState, action: RunAction): RunState {
  switch (action.type) {
    case "start":
      return { ...IDLE_RUN, status: "starting", script: action.script };
    case "runId":
      return state.runId ? state : { ...state, runId: action.runId };
    case "startFailed":
      return {
        ...state,
        status: "failed",
        error: action.error,
        messages: addMessage(state, { kind: "error", text: action.error.message }),
      };
    case "reset":
      return IDLE_RUN;
    case "event":
      // Events after the terminal one (none are expected) are ignored.
      if (!isActive(state.status)) return state;
      return applyEvent(state, action.event);
  }
}

/** "1,234 rows" plus the streaming / truncated state, for the result tab and status line. */
export function rowCountLabel(result: ResultInfo): string {
  const count = plural(result.rowCount, "row", "rows");
  if (!result.done) return `${count}…`;
  return result.truncated ? `${count} (truncated)` : count;
}
