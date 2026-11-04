/**
 * Query history: statements run from the editors, newest first (the core stores them with
 * secrets redacted). Search, filter by connection, reopen in an editor, delete, clear, and turn
 * recording off.
 */
import {
  CircleCheck,
  CircleSlash,
  CircleX,
  Eraser,
  History,
  LoaderCircle,
  type LucideIcon,
  Search,
  SearchX,
  SquareArrowOutUpRight,
  Trash2,
} from "lucide-react";
import {
  type KeyboardEvent,
  type MouseEvent,
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
} from "react";
import {
  type HistoryEntry,
  type HistoryQuery,
  type HistorySettings,
  historyClear,
  historyDelete,
  historyGetSettings,
  historyList,
  historySetSettings,
} from "../../api";
import { ICON_LG, ICON_SM } from "../../components/icons";
import { Modal } from "../../components/Modal";
import { Select } from "../../components/Select";
import { toCommandError } from "../../connections/api";
import { useConnections } from "../../connections/store";
import { onHistoryChanged } from "../events";
import { formatDateTime, formatMs } from "../format";
import { openQueryEditor } from "../layout";
import { sqlPreview } from "../positions";

/** Entries per page ("Load more" asks for the next one). */
export const HISTORY_PAGE = 200;
/** Search typing settles for this long before the list reloads. */
const SEARCH_DELAY_MS = 250;

const STATUS_LABEL: Record<HistoryEntry["status"], string> = {
  ok: "Succeeded",
  error: "Failed",
  cancelled: "Cancelled",
};

const STATUS_ICON: Record<HistoryEntry["status"], LucideIcon> = {
  ok: CircleCheck,
  error: CircleX,
  cancelled: CircleSlash,
};

/**
 * How long ago `iso` was, for the list: `just now`, `5 min ago`, `3 h ago`, `yesterday`,
 * `4 days ago`; older entries (and future ones, from a clock change) show the date and time.
 */
export function relativeTime(iso: string, now: number = Date.now()): string {
  const at = Date.parse(iso);
  if (Number.isNaN(at)) return iso;
  const seconds = Math.floor((now - at) / 1000);
  if (seconds < -60) return formatDateTime(iso);
  if (seconds < 45) return "just now";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} h ago`;
  const days = Math.round(hours / 24);
  if (days === 1) return "yesterday";
  if (days < 7) return `${days} days ago`;
  return formatDateTime(iso).slice(0, 16);
}

/**
 * The `before` bound for the page after an entry that started at `startedAt`: the end of that
 * millisecond, so entries sharing the start time are not skipped (the caller drops the ones
 * already shown). Falls back to `startedAt` itself when it does not parse.
 */
export function nextPageBefore(startedAt: string): string {
  const ms = Date.parse(startedAt);
  return Number.isNaN(ms) ? startedAt : new Date(ms + 1).toISOString();
}

export function HistoryPanel() {
  const { profiles, sessions } = useConnections();
  const [search, setSearch] = useState("");
  const [appliedSearch, setAppliedSearch] = useState("");
  const [profileId, setProfileId] = useState("");
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  const [status, setStatus] = useState<"loading" | "ok" | "error">("loading");
  const [error, setError] = useState<string | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [selected, setSelected] = useState<ReadonlySet<string>>(new Set());
  const [focused, setFocused] = useState<string | null>(null);
  /** The row under the pointer: only it and the selected or focused rows render actions. */
  const [hovered, setHovered] = useState<string | null>(null);
  const [settings, setSettings] = useState<HistorySettings | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);
  const loadSeq = useRef(0);
  const domId = useId();

  useEffect(() => {
    const timer = setTimeout(() => setAppliedSearch(search.trim()), SEARCH_DELAY_MS);
    return () => clearTimeout(timer);
  }, [search]);

  const buildQuery = useCallback(
    (before?: string): HistoryQuery => ({
      limit: HISTORY_PAGE,
      ...(appliedSearch ? { search: appliedSearch } : {}),
      ...(profileId ? { profileId } : {}),
      ...(before ? { before } : {}),
    }),
    [appliedSearch, profileId],
  );

  const reload = useCallback(async () => {
    loadSeq.current += 1;
    const seq = loadSeq.current;
    setStatus("loading");
    try {
      const page = await historyList(buildQuery());
      if (seq !== loadSeq.current) return;
      setEntries(page);
      setHasMore(page.length === HISTORY_PAGE);
      setStatus("ok");
      setError(null);
      setSelected((current) => {
        const ids = new Set(page.map((e) => e.id));
        return new Set([...current].filter((id) => ids.has(id)));
      });
    } catch (e) {
      if (seq !== loadSeq.current) return;
      setStatus("error");
      setError(toCommandError(e).message);
    }
  }, [buildQuery]);

  useEffect(() => {
    void reload();
  }, [reload]);

  useEffect(() => onHistoryChanged(() => void reload()), [reload]);

  useEffect(() => {
    historyGetSettings().then(setSettings, () => setSettings(null));
  }, []);

  async function loadMore() {
    const last = entries[entries.length - 1];
    if (!last) return;
    const seq = loadSeq.current;
    try {
      // `before` is strict and statements of one script often share a start time, so ask from
      // just after the last entry's millisecond and skip the entries already shown.
      const shown = new Set(entries.map((e) => e.id));
      const page = await historyList(buildQuery(nextPageBefore(last.startedAt)));
      if (seq !== loadSeq.current) return;
      let fresh = page.filter((e) => !shown.has(e.id));
      let more = page.length === HISTORY_PAGE;
      if (fresh.length === 0 && more) {
        // A whole page shares that millisecond: step past it rather than ask again forever.
        const next = await historyList(buildQuery(last.startedAt));
        if (seq !== loadSeq.current) return;
        fresh = next.filter((e) => !shown.has(e.id));
        more = next.length === HISTORY_PAGE;
      }
      setEntries((current) => {
        const ids = new Set(current.map((e) => e.id));
        return [...current, ...fresh.filter((e) => !ids.has(e.id))];
      });
      setHasMore(more && fresh.length > 0);
    } catch (e) {
      setError(toCommandError(e).message);
    }
  }

  async function toggleRecording() {
    if (!settings) return;
    const next = { ...settings, enabled: !settings.enabled };
    try {
      await historySetSettings(next);
      setSettings(next);
    } catch (e) {
      setError(toCommandError(e).message);
    }
  }

  async function deleteEntries(ids: string[]) {
    if (ids.length === 0) return;
    try {
      await historyDelete(ids);
      setSelected((current) => new Set([...current].filter((id) => !ids.includes(id))));
    } catch (e) {
      setError(toCommandError(e).message);
    }
    await reload();
  }

  function deleteSelected() {
    return deleteEntries([...selected]);
  }

  /** A row's hover action: does not select the row or reach its double-click. */
  function rowAction(event: MouseEvent, action: () => void) {
    event.stopPropagation();
    action();
  }

  async function clearAll() {
    setConfirmClear(false);
    try {
      await historyClear(profileId || undefined);
      setSelected(new Set());
    } catch (e) {
      setError(toCommandError(e).message);
    }
    await reload();
  }

  function reopen(entry: HistoryEntry) {
    const session = sessions.find((s) => s.profile_id === entry.profileId);
    openQueryEditor({
      sql: entry.sql,
      ...(session ? { sessionId: session.id } : {}),
      ...(entry.database ? { database: entry.database } : {}),
    });
  }

  function profileName(id: string): string {
    return profiles.find((p) => p.id === id)?.name ?? "Deleted connection";
  }

  function onClickEntry(entry: HistoryEntry, toggle: boolean) {
    setFocused(entry.id);
    setSelected((current) => {
      if (!toggle) return new Set([entry.id]);
      const next = new Set(current);
      if (next.has(entry.id)) next.delete(entry.id);
      else next.add(entry.id);
      return next;
    });
  }

  function onListKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (entries.length === 0) return;
    const index = entries.findIndex((e) => e.id === focused);
    const current = entries[index];
    let next: HistoryEntry | undefined;
    switch (event.key) {
      case "ArrowDown":
        next = entries[Math.min(index + 1, entries.length - 1)];
        break;
      case "ArrowUp":
        next = entries[Math.max(index - 1, 0)];
        break;
      case "Home":
        next = entries[0];
        break;
      case "End":
        next = entries[entries.length - 1];
        break;
      case " ":
        if (current) onClickEntry(current, true);
        break;
      case "Enter":
        if (current) reopen(current);
        break;
      case "Delete":
        void deleteSelected();
        break;
      default:
        return;
    }
    event.preventDefault();
    if (next) {
      setFocused(next.id);
      if (!event.shiftKey) setSelected(new Set([next.id]));
      document.getElementById(`${domId}-${next.id}`)?.scrollIntoView?.({ block: "nearest" });
    }
  }

  const focusedEntry = entries.find((e) => e.id === focused) ?? null;

  const now = Date.now();

  return (
    <section className="panel panel--history" aria-label="History">
      <div className="toolbar history__toolbar" role="toolbar" aria-label="History actions">
        <span className="history__search">
          <Search {...ICON_SM} className="history__search-icon" />
          <input
            type="search"
            placeholder="Search SQL"
            aria-label="Search history"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
          />
        </span>
        <Select
          aria-label="Connection filter"
          className="history__filter"
          value={profileId}
          onChange={(e) => setProfileId(e.target.value)}
        >
          <option value="">All connections</option>
          {profiles.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name}
            </option>
          ))}
        </Select>
        <span className="toolbar__separator" aria-hidden="true" />
        <button
          type="button"
          className="button button--ghost"
          disabled={!focusedEntry}
          onClick={() => focusedEntry && reopen(focusedEntry)}
          title="Open the focused query in a new editor (Enter)"
        >
          <SquareArrowOutUpRight {...ICON_SM} />
          Open in editor
        </button>
        <button
          type="button"
          className="button button--ghost"
          disabled={selected.size === 0}
          onClick={() => void deleteSelected()}
          title="Delete the selected queries (Delete)"
        >
          <Trash2 {...ICON_SM} />
          Delete
        </button>
        <button
          type="button"
          className="button button--ghost"
          disabled={entries.length === 0}
          onClick={() => setConfirmClear(true)}
          title="Remove every saved query of the connection shown"
        >
          <Eraser {...ICON_SM} />
          Clear…
        </button>
        <label className="checkbox history__recording">
          <input
            type="checkbox"
            checked={settings?.enabled ?? false}
            disabled={!settings}
            onChange={() => void toggleRecording()}
          />
          Record history
        </label>
      </div>

      {settings && !settings.enabled ? (
        <p className="history__notice" role="status">
          <CircleSlash {...ICON_SM} />
          Recording is off: new queries are not saved.
        </p>
      ) : null}

      {error ? (
        <div className="test-result test-result--error" role="alert">
          <p className="test-result__message">{error}</p>
        </div>
      ) : null}

      {entries.length === 0 ? (
        <div className="empty-state">
          <span className="empty-state__icon">
            {status === "loading" ? (
              <LoaderCircle {...ICON_LG} className="spin" />
            ) : appliedSearch ? (
              <SearchX {...ICON_LG} />
            ) : (
              <History {...ICON_LG} />
            )}
          </span>
          <p className="empty-state__title">
            {status === "loading" ? "Loading history…" : "No history"}
          </p>
          <p className="empty-state__hint">
            {appliedSearch ? "No query matches the search." : "Queries you run appear here."}
          </p>
        </div>
      ) : (
        <div
          className="history__list"
          role="listbox"
          aria-label="Query history"
          aria-multiselectable="true"
          tabIndex={0}
          aria-activedescendant={focusedEntry ? `${domId}-${focusedEntry.id}` : undefined}
          onKeyDown={onListKeyDown}
          onMouseLeave={() => setHovered(null)}
        >
          {entries.map((entry) => {
            const StatusIcon = STATUS_ICON[entry.status];
            return (
              // biome-ignore lint/a11y/useKeyWithClickEvents: the listbox handles the keyboard.
              <div
                key={entry.id}
                id={`${domId}-${entry.id}`}
                role="option"
                tabIndex={-1}
                aria-selected={selected.has(entry.id)}
                className={`history__item${entry.id === focused ? " history__item--focused" : ""}`}
                onClick={(e) => onClickEntry(entry, e.ctrlKey || e.metaKey)}
                onDoubleClick={() => reopen(entry)}
                onMouseEnter={() => setHovered(entry.id)}
                title={entry.sql.length > 2000 ? `${entry.sql.slice(0, 2000)}…` : entry.sql}
              >
                <span
                  className={`history__status history__status--${entry.status}`}
                  role="img"
                  aria-label={STATUS_LABEL[entry.status]}
                >
                  <StatusIcon {...ICON_SM} />
                </span>
                <span className="history__body">
                  <code className="history__sql">{sqlPreview(entry.sql, 300)}</code>
                  <span className="history__meta">
                    <time
                      className="history__time"
                      dateTime={entry.startedAt}
                      title={formatDateTime(entry.startedAt)}
                    >
                      {relativeTime(entry.startedAt, now)}
                    </time>
                    <span className="history__chip" title="Duration">
                      {formatMs(entry.durationMs)}
                    </span>
                    {entry.rows !== undefined ? (
                      <span className="history__chip" title="Rows">
                        {`${entry.rows.toLocaleString("en-US")} rows`}
                      </span>
                    ) : null}
                    <span className="history__target">
                      {profileName(entry.profileId)}
                      {entry.database ? ` / ${entry.database}` : ""}
                    </span>
                  </span>
                  {entry.error ? <span className="history__error">{entry.error}</span> : null}
                </span>
                <span className="history__actions">
                  {selected.has(entry.id) || entry.id === focused || entry.id === hovered ? (
                    <>
                      <button
                        type="button"
                        className="icon-button icon-button--small"
                        tabIndex={-1}
                        aria-label="Reopen in editor"
                        title="Reopen in editor (Enter)"
                        onClick={(e) => rowAction(e, () => reopen(entry))}
                        onDoubleClick={(e) => e.stopPropagation()}
                      >
                        <SquareArrowOutUpRight {...ICON_SM} />
                      </button>
                      <button
                        type="button"
                        className="icon-button icon-button--small history__delete"
                        tabIndex={-1}
                        aria-label="Delete from history"
                        title="Delete from history"
                        onClick={(e) => rowAction(e, () => void deleteEntries([entry.id]))}
                        onDoubleClick={(e) => e.stopPropagation()}
                      >
                        <Trash2 {...ICON_SM} />
                      </button>
                    </>
                  ) : null}
                </span>
              </div>
            );
          })}
        </div>
      )}
      {hasMore && entries.length > 0 ? (
        <button
          type="button"
          className="button button--ghost button--small history__more"
          onClick={() => void loadMore()}
        >
          Load more
        </button>
      ) : null}

      {confirmClear ? (
        <Modal
          title="Clear history"
          onClose={() => setConfirmClear(false)}
          footer={
            <>
              <button
                type="button"
                className="button"
                data-autofocus
                onClick={() => setConfirmClear(false)}
              >
                Cancel
              </button>
              <button
                type="button"
                className="button button--danger"
                onClick={() => void clearAll()}
              >
                <Eraser {...ICON_SM} />
                Clear
              </button>
            </>
          }
        >
          <p>
            {profileId
              ? `Remove every saved query of ${profileName(profileId)}?`
              : "Remove every saved query of all connections?"}{" "}
            This cannot be undone.
          </p>
        </Modal>
      ) : null}
    </section>
  );
}
