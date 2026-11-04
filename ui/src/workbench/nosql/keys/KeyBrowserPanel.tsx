/**
 * The Redis key browser (docs/m7-contracts.md §3.2, §5.2, §5.4): a database picker, a pattern
 * and type filter, the namespace tree, the key list of a `SCAN` (never `KEYS`; `key_scan_open`
 * streams every match into the result store, the list pages it) and the typed value editor of
 * the selected key. No FLUSHDB / FLUSHALL here: the console asks for those.
 */
import {
  ChevronRight,
  CircleAlert,
  FolderTree,
  KeyRound,
  LoaderCircle,
  Play,
  Plus,
  Search,
  Square,
} from "lucide-react";
import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import {
  type BinaryText,
  displayBinary,
  type KeyAddress,
  type KeyInfo,
  type KeyScanRequest,
  type KeyType,
  keyInfo,
  keyScanOpen,
  queryCancel,
  type TreeNode,
  treeChildren,
} from "../../../api";
import { ICON, ICON_LG, ICON_SM } from "../../../components/icons";
import { Select } from "../../../components/Select";
import { toCommandError } from "../../../connections/api";
import { useConnections } from "../../../connections/store";
import type { KeyBrowserParams } from "../../layout";
import { ResultPager } from "../../results/pager";
import { timedQueryPage } from "../../results/perf";
import { IDLE_RUN, isActive, runReducer } from "../../results/runState";
import { closeEditorLater, editorReady } from "../editorClose";
import { EditorRunSlot } from "../runSlot";
import { KeyList, type KeyRow } from "./KeyList";
import { KeyValueEditor } from "./KeyValueEditor";
import { KEY_TYPE_LABEL, KEY_TYPES, prefixPattern } from "./keyModel";
import { NewKeyDialog } from "./NewKeyDialog";

/** Key infos read at once for the size column. */
const SIZE_CONCURRENCY = 4;

export function KeyBrowserPanel({
  editorId,
  sessionId,
  database: initialDatabase,
  pattern: initialPattern,
  openKey: initialKey,
  request,
}: KeyBrowserParams) {
  const session = useConnections().sessions.find((s) => s.id === sessionId);
  const [databases, setDatabases] = useState<string[]>([]);
  const [database, setDatabase] = useState(initialDatabase ?? session?.database ?? "0");
  const [pattern, setPattern] = useState(initialPattern ?? "");
  const [type, setType] = useState<Exclude<KeyType, "other"> | "">("");
  const [run, dispatch] = useReducer(runReducer, IDLE_RUN);
  const [selected, setSelected] = useState<{ row: number | null; address: KeyAddress } | null>(
    null,
  );
  const [sizes, setSizes] = useState<ReadonlyMap<string, KeyInfo | "error">>(new Map());
  const [newKey, setNewKey] = useState(false);
  const [namespaces, setNamespaces] = useState<
    ReadonlyMap<string, TreeNode[] | "loading" | string>
  >(new Map());
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set());
  const seq = useRef(0);
  const runIdRef = useRef<string | null>(null);
  const pagerRef = useRef<{ runId: string; pager: ResultPager } | null>(null);
  const sizesInFlight = useRef(new Set<string>());
  const sizesQueue = useRef<KeyRow[]>([]);
  const sizesGen = useRef(0);
  /** The scan editor runs one scan at a time: a new scan stops the one still streaming. */
  const slot = useRef(new EditorRunSlot());

  // The databases of the connection (database rows of the tree).
  useEffect(() => {
    let alive = true;
    treeChildren(sessionId, { type: "connection" }).then(
      (nodes) => {
        if (!alive) return;
        setDatabases(
          nodes.flatMap((n) =>
            n.node.type === "object" && n.node.object.kind === "database"
              ? [n.node.object.name]
              : [],
          ),
        );
      },
      () => {},
    );
    return () => {
      alive = false;
    };
  }, [sessionId]);

  const scan = useCallback(
    async (db: string, match: string, keyType: string) => {
      seq.current += 1;
      const mine = seq.current;
      runIdRef.current = null;
      sizesGen.current += 1;
      sizesQueue.current = [];
      sizesInFlight.current.clear();
      setSizes(new Map());
      const req: KeyScanRequest = { database: db };
      if (match.trim() !== "") req.pattern = match.trim();
      if (keyType !== "") req.keyType = keyType as Exclude<KeyType, "other">;
      dispatch({ type: "start", script: `SCAN 0 MATCH ${req.pattern ?? "*"}` });
      const ticket = await slot.current.take();
      await editorReady(editorId);
      if (mine !== seq.current) {
        ticket.release();
        return;
      }
      try {
        const id = await keyScanOpen(sessionId, editorId, req, (event) => {
          ticket.event(event);
          if (mine === seq.current) dispatch({ type: "event", event });
        });
        ticket.started(id);
        if (mine !== seq.current) return;
        runIdRef.current = id;
        dispatch({ type: "runId", runId: id });
      } catch (error) {
        ticket.release();
        if (mine === seq.current) dispatch({ type: "startFailed", error: toCommandError(error) });
      }
    },
    [sessionId, editorId],
  );

  // First scan, and every request from the tree (a namespace's pattern, a key to open).
  const requestRef = useRef<number | undefined>(undefined);
  // biome-ignore lint/correctness/useExhaustiveDependencies: only a new request re-applies the tree's pattern and key.
  useEffect(() => {
    if (requestRef.current === request && requestRef.current !== undefined) return;
    requestRef.current = request;
    const match = initialPattern ?? "";
    // The tab may show another database (its picker): the request names the one to use.
    const db = initialDatabase ?? session?.database ?? "0";
    if (db !== database) {
      setDatabase(db);
      setSelected(null);
    }
    setPattern(match);
    void scan(db, match, type);
    if (initialKey) setSelected({ row: null, address: { database: db, key: initialKey } });
    return () => {
      // A remount (StrictMode, dockview moving the tab) scans again.
      requestRef.current = undefined;
    };
  }, [request]);

  useEffect(
    () => () => {
      seq.current += 1;
      closeEditorLater(editorId);
      pagerRef.current?.pager.dispose();
    },
    [editorId],
  );

  const result = run.results[0];
  const runId = run.runId;
  let pager: ResultPager | null = null;
  if (runId && result) {
    if (pagerRef.current?.runId !== runId) {
      pagerRef.current?.pager.dispose();
      const index = result.resultIndex;
      pagerRef.current = {
        runId,
        pager: new ResultPager((offset, limit) => timedQueryPage(runId, index, offset, limit)),
      };
    }
    pager = pagerRef.current.pager;
  }

  // --- sizes of the keys on screen ---------------------------------------------------------

  const pumpSizes = useCallback(() => {
    const gen = sizesGen.current;
    while (sizesInFlight.current.size < SIZE_CONCURRENCY && sizesQueue.current.length > 0) {
      const row = sizesQueue.current.shift();
      if (!row || sizesInFlight.current.has(row.text)) continue;
      sizesInFlight.current.add(row.text);
      const done = (value: KeyInfo | "error") => {
        if (gen !== sizesGen.current) return;
        sizesInFlight.current.delete(row.text);
        setSizes((current) => new Map(current).set(row.text, value));
        pumpSizes();
      };
      keyInfo(sessionId, { database, key: row.key }).then(done, () => done("error"));
    }
  }, [sessionId, database]);

  const onVisible = useCallback(
    (rows: KeyRow[]) => {
      sizesQueue.current = rows.filter(
        (r) => !sizes.has(r.text) && !sizesInFlight.current.has(r.text),
      );
      pumpSizes();
    },
    [sizes, pumpSizes],
  );

  // --- namespaces -----------------------------------------------------------------------------

  const loadNamespaces = useCallback(
    (id: string, node: TreeNode["node"]) => {
      setNamespaces((m) => new Map(m).set(id, "loading"));
      treeChildren(sessionId, node).then(
        (nodes) =>
          setNamespaces((m) =>
            new Map(m).set(
              id,
              nodes.filter((n) => n.expandable),
            ),
          ),
        (error) => setNamespaces((m) => new Map(m).set(id, toCommandError(error).message)),
      );
    },
    [sessionId],
  );

  const rootId = `db:${database}`;
  useEffect(() => {
    setExpanded(new Set());
    setNamespaces(new Map());
    loadNamespaces(rootId, { type: "object", object: { kind: "database", name: database } });
  }, [rootId, database, loadNamespaces]);

  function toggleNamespace(node: TreeNode) {
    setExpanded((current) => {
      const next = new Set(current);
      if (next.has(node.key)) next.delete(node.key);
      else {
        next.add(node.key);
        if (!namespaces.has(node.key)) loadNamespaces(node.key, node.node);
      }
      return next;
    });
  }

  function browseNamespace(node: TreeNode) {
    if (node.node.type === "object" && node.node.object.kind === "schema") {
      const match = prefixPattern(node.node.object.name);
      setPattern(match);
      void scan(database, match, type);
    }
    if (!expanded.has(node.key)) toggleNamespace(node);
  }

  function renderNamespaces(id: string, level: number): React.ReactNode {
    const children = namespaces.get(id);
    if (children === undefined) return null;
    if (children === "loading") {
      return (
        <li className="namespace__row namespace__row--muted" style={{ paddingLeft: level * 14 }}>
          <LoaderCircle {...ICON_SM} className="spin" /> Loading…
        </li>
      );
    }
    if (typeof children === "string") {
      return (
        <li className="namespace__row namespace__row--error" style={{ paddingLeft: level * 14 }}>
          <CircleAlert {...ICON_SM} /> {children}
        </li>
      );
    }
    return children.map((node) => {
      const open = expanded.has(node.key);
      const prefix =
        node.node.type === "object" && node.node.object.kind === "schema"
          ? prefixPattern(node.node.object.name)
          : null;
      return (
        <li key={node.key} className="namespace__item">
          <div
            className={`namespace__row${prefix !== null && prefix === pattern ? " namespace__row--current" : ""}`}
            style={{ paddingLeft: level * 14 }}
          >
            <button
              type="button"
              className={`namespace__twisty${open ? " namespace__twisty--open" : ""}`}
              aria-label={`${open ? "Collapse" : "Expand"} ${node.label}`}
              aria-expanded={open}
              onClick={() => toggleNamespace(node)}
            >
              <ChevronRight {...ICON_SM} />
            </button>
            <button
              type="button"
              className="namespace__label"
              title={prefix ? `Scan ${prefix}` : node.label}
              onClick={() => browseNamespace(node)}
            >
              <FolderTree {...ICON_SM} className="namespace__icon" />
              {node.label}
            </button>
          </div>
          {open ? (
            <ul className="namespace__list">{renderNamespaces(node.key, level + 1)}</ul>
          ) : null}
        </li>
      );
    });
  }

  // --- render ---------------------------------------------------------------------------------

  const active = isActive(run.status);
  const count = result?.rowCount ?? 0;
  let status = "";
  if (run.status === "failed") status = `Failed: ${run.error?.message ?? "unknown error"}`;
  else if (run.status === "cancelled") status = `Stopped at ${count.toLocaleString("en-US")} keys`;
  else if (result) {
    status = `${count.toLocaleString("en-US")} ${count === 1 ? "key" : "keys"}${active ? "…" : ""}`;
  } else if (active) status = "Scanning…";
  const prefix =
    pattern.endsWith("*") && !/[?[\\]/.test(pattern.slice(0, -1)) ? pattern.slice(0, -1) : "";

  return (
    <section
      className="key-browser"
      aria-label={`Keys of database ${database}`}
      data-testid="key-browser"
    >
      <div className="toolbar table-data__toolbar" role="toolbar" aria-label="Key browser actions">
        <label className="query-editor__picker" title="Logical database">
          <span className="visually-hidden">Database</span>
          <Select
            aria-label="Database"
            value={database}
            disabled={active}
            onChange={(e) => {
              setDatabase(e.target.value);
              setSelected(null);
              void scan(e.target.value, pattern, type);
            }}
          >
            {(databases.includes(database) ? databases : [database, ...databases]).map((db) => (
              <option key={db} value={db}>
                db{db}
              </option>
            ))}
          </Select>
        </label>
        <form
          className="key-browser__filter"
          aria-label="Find keys"
          onSubmit={(e) => {
            e.preventDefault();
            void scan(database, pattern, type);
          }}
        >
          <Search {...ICON_SM} className="key-browser__search-icon" />
          <input
            type="text"
            aria-label="Key pattern"
            data-testid="key-pattern"
            placeholder="Pattern, e.g. user:*"
            spellCheck={false}
            value={pattern}
            onChange={(e) => setPattern(e.target.value)}
          />
          <Select
            aria-label="Key type"
            value={type}
            onChange={(e) => {
              const next = e.target.value as Exclude<KeyType, "other"> | "";
              setType(next);
              void scan(database, pattern, next);
            }}
          >
            <option value="">All types</option>
            {KEY_TYPES.map((t) => (
              <option key={t} value={t}>
                {KEY_TYPE_LABEL[t]}
              </option>
            ))}
          </Select>
          <button
            type="submit"
            className="button button--primary"
            data-testid="key-scan"
            disabled={active}
            title="Scan the keys that match (SCAN, never KEYS)"
          >
            <Play {...ICON_SM} fill="currentColor" />
            Scan
          </button>
        </form>
        <button
          type="button"
          className="icon-button"
          aria-label="Stop"
          title="Stop scanning"
          disabled={!active}
          onClick={() => {
            const id = runIdRef.current;
            if (id) void queryCancel(id).catch(() => {});
          }}
        >
          <Square {...ICON} />
        </button>
        <span className="toolbar__separator" aria-hidden="true" />
        <button type="button" className="button button--ghost" onClick={() => setNewKey(true)}>
          <Plus {...ICON} />
          New key
        </button>
        <span
          className="query-editor__status table-data__status"
          role="status"
          aria-live="polite"
          data-testid="key-status"
        >
          {status}
        </span>
      </div>
      <div className="key-browser__body">
        <nav className="key-browser__namespaces" aria-label="Namespaces">
          <button
            type="button"
            className={`namespace__row namespace__all${pattern === "" || pattern === "*" ? " namespace__row--current" : ""}`}
            onClick={() => {
              setPattern("");
              void scan(database, "", type);
            }}
          >
            <KeyRound {...ICON_SM} className="namespace__icon" />
            All keys
          </button>
          <ul className="namespace__list">{renderNamespaces(rootId, 0)}</ul>
        </nav>
        <div className="key-browser__keys">
          {run.status === "failed" && !result ? (
            <div className="empty-state" role="alert">
              <span className="empty-state__icon">
                <CircleAlert {...ICON_LG} />
              </span>
              <p className="empty-state__title">Could not scan the keys</p>
              <p className="empty-state__hint">{run.error?.message}</p>
            </div>
          ) : result && pager ? (
            count === 0 && !active ? (
              <p className="results__empty">No keys match.</p>
            ) : (
              <KeyList
                pager={pager}
                columns={result.columns}
                rowCount={count}
                selected={selected?.row ?? null}
                sizes={sizes}
                onVisible={onVisible}
                onSelect={(row, key) => setSelected({ row, address: { database, key: key.key } })}
              />
            )
          ) : (
            <div className="table-data__loading" role="status">
              <LoaderCircle {...ICON} className="spin" />
              Scanning…
            </div>
          )}
        </div>
        <div className="key-browser__value">
          {selected ? (
            <KeyValueEditor
              key={`${selected.address.database}:${displayBinary(selected.address.key)}`}
              sessionId={sessionId}
              editorId={`${editorId}-value`}
              address={selected.address}
              onChanged={(change) => {
                if (change.kind === "deleted") {
                  setSelected(null);
                  void scan(database, pattern, type);
                } else if (change.kind === "renamed") {
                  setSelected({ row: null, address: { database, key: change.key } });
                  void scan(database, pattern, type);
                } else if (change.info) {
                  const info = change.info;
                  setSizes((m) => new Map(m).set(displayBinary(selected.address.key), info));
                }
              }}
            />
          ) : (
            <div className="empty-state">
              <span className="empty-state__icon" aria-hidden="true">
                <KeyRound {...ICON_LG} />
              </span>
              <p className="empty-state__title">No key selected</p>
              <p className="empty-state__hint">Pick a key to see and edit its value.</p>
            </div>
          )}
        </div>
      </div>
      {newKey ? (
        <NewKeyDialog
          sessionId={sessionId}
          database={database}
          prefix={prefix}
          onClose={() => setNewKey(false)}
          onCreated={(name) => {
            setNewKey(false);
            const key: BinaryText = { encoding: "utf8", text: name };
            setSelected({ row: null, address: { database, key } });
            void scan(database, pattern, type);
          }}
        />
      ) : null}
    </section>
  );
}
