/**
 * The lower half of a query editor: one tab per result set of the current run plus a Messages
 * tab (notices, rows affected, timings, errors). Each result tab pages its rows from the core
 * through its own {@link ResultPager}; hidden tabs drop their cached pages.
 */
import {
  CircleCheck,
  CircleCheckBig,
  CircleSlash,
  CircleX,
  Download,
  Info,
  LoaderCircle,
  type LucideIcon,
  MessageSquareText,
  Play,
  Rows3,
  Table2,
  Timer,
  TriangleAlert,
} from "lucide-react";
import { type KeyboardEvent, useEffect, useRef, useState } from "react";
import { ICON_LG, ICON_SM } from "../../components/icons";
import { formatMs } from "../format";
import { ResultPager } from "./pager";
import { timedQueryPage } from "./perf";
import { ResultGrid } from "./ResultGrid";
import {
  isActive,
  type ResultInfo,
  type RunMessage,
  type RunState,
  rowCountLabel,
} from "./runState";

type TabId = "messages" | number;

type Tone = "info" | "success" | "warning" | "danger" | "muted";

/** A message's severity icon, its tone and what it says to assistive technology. */
function messageIcon(m: RunMessage): { Icon: LucideIcon; tone: Tone; label: string } {
  switch (m.kind) {
    case "error":
      return { Icon: CircleX, tone: "danger", label: "Error" };
    case "ok":
      return { Icon: CircleCheck, tone: "success", label: "Done" };
    case "summary":
      return { Icon: CircleCheckBig, tone: "success", label: "Summary" };
    case "notice":
      if (m.level === "warning") return { Icon: TriangleAlert, tone: "warning", label: "Warning" };
      return { Icon: Info, tone: m.level === "debug" ? "muted" : "info", label: "Notice" };
  }
}

function Messages({
  messages,
  dropped,
  status,
  note,
}: {
  messages: RunMessage[];
  dropped: number;
  status: RunState["status"];
  note: string | null;
}) {
  const noteLine =
    note && status !== "idle" ? (
      <p className="messages__note" role="note" data-testid="messages-note">
        <Info {...ICON_SM} className="messages__note-icon" />
        <span>{note}</span>
      </p>
    ) : null;
  if (messages.length === 0) {
    const idle = status === "idle";
    return (
      <>
        {noteLine}
        <div className="empty-state results__empty-state">
          <span className="empty-state__icon">
            {idle ? <Play {...ICON_LG} /> : <LoaderCircle {...ICON_LG} className="spin" />}
          </span>
          <p className="empty-state__title">
            {idle ? "Run a query to see its results here." : "No messages yet."}
          </p>
          {idle ? (
            <p className="empty-state__hint">
              <kbd className="kbd">Ctrl</kbd> <kbd className="kbd">Enter</kbd> runs the statement at
              the cursor, <kbd className="kbd">Ctrl</kbd> <kbd className="kbd">Shift</kbd>{" "}
              <kbd className="kbd">Enter</kbd> the whole script.
            </p>
          ) : null}
        </div>
      </>
    );
  }
  return (
    <>
      {noteLine}
      {dropped > 0 ? (
        <p className="messages__dropped" data-testid="messages-dropped">
          <Info {...ICON_SM} className="messages__note-icon" />
          <span>
            {dropped.toLocaleString("en-US")} earlier {dropped === 1 ? "message" : "messages"} not
            shown
          </span>
        </p>
      ) : null}
      <ol className="messages" aria-label="Messages">
        {messages.map((m) => {
          const { Icon, tone, label } = messageIcon(m);
          return (
            <li
              key={m.id}
              className={`messages__item messages__item--${m.kind}${m.level ? ` messages__item--${m.level}` : ""}`}
              data-tone={tone}
            >
              <span className="messages__icon" role="img" aria-label={label}>
                <Icon {...ICON_SM} />
              </span>
              <span className="messages__body">
                {m.statement ? <code className="messages__statement">{m.statement}</code> : null}
                <span className="messages__text">{m.text}</span>
              </span>
              {m.elapsedMs !== undefined ? (
                <span className="messages__time">{formatMs(m.elapsedMs)}</span>
              ) : null}
            </li>
          );
        })}
      </ol>
    </>
  );
}

/** Re-renders when `pager` loads rows or a page request fails (the pager is not React state). */
function usePagerUpdates(pager: ResultPager | null): void {
  const [, setTick] = useState(0);
  useEffect(() => {
    if (!pager) return;
    let lastError = pager.error;
    return pager.onLoad(() => {
      if (pager.error === lastError) return;
      lastError = pager.error;
      setTick((t) => t + 1);
    });
  }, [pager]);
}

export interface ResultsAreaProps {
  run: RunState;
  /** A note for the Messages tab about what the engine cannot report (SQL Server PRINT). */
  messagesNote?: string | null | undefined;
  /** Exports every row of a result (offered once the run has ended). */
  onExport?: ((result: ResultInfo) => void) | undefined;
}

export function ResultsArea({ run, messagesNote = null, onExport }: ResultsAreaProps) {
  // The tab the user picked, for the run it was picked in (a new run starts on its first result).
  const [picked, setPicked] = useState<{ runId: string | null; tab: TabId } | null>(null);
  const pagers = useRef(new Map<number, ResultPager>());
  /** The last copy notice, for the result tab it came from. */
  const [notice, setNotice] = useState<{ key: string; text: string } | null>(null);
  const pagersRun = useRef<string | null>(null);

  // A new run (or none) starts with fresh pagers.
  if (pagersRun.current !== run.runId) {
    for (const pager of pagers.current.values()) pager.dispose();
    pagers.current.clear();
    pagersRun.current = run.runId;
  }
  useEffect(
    () => () => {
      for (const pager of pagers.current.values()) pager.dispose();
      pagers.current.clear();
    },
    [],
  );
  const active = picked && picked.runId === run.runId ? picked.tab : null;
  const setActive = (tab: TabId) => setPicked({ runId: run.runId, tab });

  const first = run.results[0];
  const shown: TabId = active ?? first?.resultIndex ?? "messages";

  // Hidden result tabs keep no rows in memory.
  useEffect(() => {
    for (const [index, pager] of pagers.current) if (index !== shown) pager.release();
  }, [shown]);

  function pagerFor(result: ResultInfo): ResultPager | null {
    const runId = run.runId;
    if (!runId) return null;
    let pager = pagers.current.get(result.resultIndex);
    if (!pager) {
      pager = new ResultPager((offset, limit) =>
        timedQueryPage(runId, result.resultIndex, offset, limit),
      );
      pagers.current.set(result.resultIndex, pager);
    }
    return pager;
  }

  const tabs: TabId[] = [...run.results.map((r) => r.resultIndex), "messages"];

  function onTabKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    const index = tabs.indexOf(shown);
    let next: number | undefined;
    if (event.key === "ArrowRight") next = (index + 1) % tabs.length;
    else if (event.key === "ArrowLeft") next = (index - 1 + tabs.length) % tabs.length;
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = tabs.length - 1;
    if (next === undefined) return;
    event.preventDefault();
    const tab = tabs[next];
    if (tab !== undefined) {
      setActive(tab);
      const el = event.currentTarget.querySelector<HTMLElement>(`[data-tab="${String(tab)}"]`);
      el?.focus();
    }
  }

  const activeResult =
    typeof shown === "number" ? run.results.find((r) => r.resultIndex === shown) : undefined;
  const errors = run.errorCount;
  const pager = activeResult ? pagerFor(activeResult) : null;
  usePagerUpdates(pager);
  const gridKey = activeResult ? `${run.runId}:${activeResult.resultIndex}` : "";
  const shownNotice = notice && notice.key === gridKey ? notice.text : null;
  // The time the result's statement took, once it has ended.
  const activeElapsed = activeResult
    ? run.messages.find(
        (m) =>
          m.statementIndex === activeResult.statementIndex &&
          m.elapsedMs !== undefined &&
          (m.kind === "ok" || m.kind === "error"),
      )?.elapsedMs
    : undefined;

  return (
    <section className="results" aria-label="Results">
      <div
        className="results__tabs"
        role="tablist"
        aria-label="Result sets"
        onKeyDown={onTabKeyDown}
      >
        {run.results.map((r) => (
          <button
            key={r.resultIndex}
            type="button"
            role="tab"
            data-tab={r.resultIndex}
            aria-selected={shown === r.resultIndex}
            tabIndex={shown === r.resultIndex ? 0 : -1}
            className="results__tab"
            onClick={() => setActive(r.resultIndex)}
          >
            {r.done ? (
              <Table2 {...ICON_SM} className="results__tab-icon" />
            ) : (
              <LoaderCircle {...ICON_SM} className="results__tab-icon spin" />
            )}
            Result {r.resultIndex + 1}
            <span className="results__tab-count">{rowCountLabel(r)}</span>
          </button>
        ))}
        <button
          type="button"
          role="tab"
          data-tab="messages"
          aria-selected={shown === "messages"}
          tabIndex={shown === "messages" ? 0 : -1}
          className="results__tab"
          onClick={() => setActive("messages")}
        >
          <MessageSquareText {...ICON_SM} className="results__tab-icon" />
          Messages
          {errors > 0 ? <span className="results__tab-badge">{errors}</span> : null}
        </button>
      </div>
      <div className="results__body" role="tabpanel">
        {activeResult && pager ? (
          <>
            <div className="results__status" aria-live="polite" data-testid="result-status">
              <span className="results__count">
                {activeResult.done ? (
                  <Rows3 {...ICON_SM} className="results__status-icon" />
                ) : (
                  <LoaderCircle {...ICON_SM} className="results__status-icon spin" />
                )}
                <span data-testid="row-counter">{rowCountLabel(activeResult)}</span>
              </span>
              {activeElapsed !== undefined ? (
                <span className="results__meta" title="Time the statement took">
                  <Timer {...ICON_SM} />
                  {formatMs(activeElapsed)}
                </span>
              ) : null}
              {activeResult.columns.length === 0 ? (
                <span className="results__meta">no columns</span>
              ) : null}
              {activeResult.done ? null : <span className="badge badge--info">Streaming</span>}
              {activeResult.truncated && run.status === "cancelled" ? (
                <span className="badge badge--warning">
                  <CircleSlash {...ICON_SM} />
                  Cancelled
                </span>
              ) : null}
              {activeResult.truncated && run.status === "failed" ? (
                <span className="badge badge--danger">
                  <CircleX {...ICON_SM} />
                  Failed
                </span>
              ) : null}
              {pager.error ? (
                <span className="results__problem">
                  <TriangleAlert {...ICON_SM} />
                  page failed: {pager.error}
                </span>
              ) : null}
              {shownNotice ? (
                <span className="results__notice">
                  <Info {...ICON_SM} />
                  {shownNotice}
                </span>
              ) : null}
              {onExport ? (
                <button
                  type="button"
                  className="button button--small button--ghost results__export"
                  disabled={isActive(run.status) || activeResult.columns.length === 0}
                  title={
                    isActive(run.status)
                      ? "Available once the run has ended"
                      : "Export every row of this result"
                  }
                  onClick={() => onExport(activeResult)}
                >
                  <Download {...ICON_SM} />
                  Export…
                </button>
              ) : null}
            </div>
            <div className="results__grid">
              <ResultGrid
                key={gridKey}
                pager={pager}
                columns={activeResult.columns}
                rowCount={activeResult.rowCount}
                onNotice={(text) => setNotice({ key: gridKey, text })}
              />
            </div>
          </>
        ) : (
          <Messages
            messages={run.messages}
            dropped={run.droppedMessages}
            status={run.status}
            note={messagesNote}
          />
        )}
      </div>
    </section>
  );
}
