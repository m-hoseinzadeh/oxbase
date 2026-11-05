/**
 * The status bar's session counter: how many database sessions Oxbase holds, and on click
 * where each connection's sessions go (the object tree, idle query tabs, running queries, the
 * rest). Refreshed every {@link REFRESH_MS} while a connection is open (`session_usage` reads
 * the core's state, no network access).
 */
import { Layers, X } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import { ICON_SM } from "../components/icons";
import { sessionUsage } from "./api";
import { useConnections } from "./store";
import type { SessionUsage } from "./types";

const REFRESH_MS = 2000;

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

export function SessionCounter() {
  const { sessions } = useConnections();
  const [usage, setUsage] = useState<SessionUsage[]>([]);
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  const popoverId = useId();
  const connected = sessions.length > 0;

  useEffect(() => {
    if (!connected) {
      setUsage([]);
      setOpen(false);
      return;
    }
    let live = true;
    const load = () =>
      sessionUsage().then(
        (u) => {
          if (live) setUsage(u);
        },
        () => {},
      );
    void load();
    const timer = setInterval(() => void load(), REFRESH_MS);
    return () => {
      live = false;
      clearInterval(timer);
    };
  }, [connected]);

  useEffect(() => {
    if (!open) return;
    const onDown = (event: MouseEvent) => {
      if (!root.current?.contains(event.target as Node)) setOpen(false);
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      setOpen(false);
      button.current?.focus();
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  if (!connected) return null;
  // Until the first answer, each open connection holds at least its own session.
  const total = usage.length > 0 ? usage.reduce((n, u) => n + u.total, 0) : sessions.length;

  function close() {
    setOpen(false);
    button.current?.focus();
  }

  return (
    <div ref={root} className="session-counter">
      <button
        ref={button}
        type="button"
        className="status-bar__button"
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-controls={open ? popoverId : undefined}
        title="Database sessions Oxbase holds; click for details"
        data-testid="session-counter"
        onClick={() => setOpen((o) => !o)}
      >
        <Layers {...ICON_SM} />
        {plural(total, "session", "sessions")}
      </button>
      {open ? (
        <div
          id={popoverId}
          className="session-popover"
          role="dialog"
          aria-label="Database sessions"
        >
          <header className="session-popover__header">
            <h2 className="session-popover__title">Database sessions</h2>
            <button
              type="button"
              className="icon-button"
              aria-label="Close"
              title="Close (Esc)"
              onClick={close}
            >
              <X {...ICON_SM} />
            </button>
          </header>
          <ul className="session-popover__list">
            {usage.map((u) => (
              <li key={u.sessionId} className="session-popover__connection">
                <div className="session-popover__head">
                  <span className="session-popover__name">{u.profileName}</span>
                  <span className="badge">
                    {u.sessionMode === "per_run" ? "Connects for each run" : "Session per tab"}
                  </span>
                  <span className="session-popover__total">{u.total}</span>
                </div>
                <dl className="session-popover__rows">
                  <div>
                    <dt>Object tree</dt>
                    <dd>1</dd>
                  </div>
                  {u.idleTabs > 0 ? (
                    <div>
                      <dt>Query tabs, idle</dt>
                      <dd>{u.idleTabs}</dd>
                    </div>
                  ) : null}
                  {u.running > 0 ? (
                    <div>
                      <dt>Running queries</dt>
                      <dd>{u.running}</dd>
                    </div>
                  ) : null}
                  {u.other > 0 ? (
                    <div>
                      <dt>Other (tree databases, edits, imports)</dt>
                      <dd>{u.other}</dd>
                    </div>
                  ) : null}
                </dl>
              </li>
            ))}
          </ul>
          <p className="session-popover__hint">
            A connection set to “Connect for each run” holds sessions only while queries run or
            edits are saved. Change it in the connection’s settings.
          </p>
        </div>
      ) : null}
    </div>
  );
}
