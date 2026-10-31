/**
 * The object tree: every open connection, with its databases, schemas, per-kind folders and
 * objects below it. Children are listed on demand (`tree_children`) when a node is first
 * expanded; a collapsed node's pending request is cancelled. Keyboard: arrows move and
 * expand/collapse, Home/End jump, Enter toggles (or retries a failed load), F5 refreshes.
 */
import { type KeyboardEvent, useCallback, useEffect, useId, useRef, useState } from "react";
import { type TreeNodeRef, treeChildren } from "../../api";
import { cancelRequest, newRequestId, toCommandError } from "../../connections/api";
import { useConnections } from "../../connections/store";
import { openQueryEditor } from "../layout";
import {
  type ChildrenState,
  cachedDescendants,
  databaseOf,
  flattenTree,
  type Row,
} from "./treeModel";

type NodeRow = Extract<Row, { type: "node" }>;

function isNavigable(row: Row): boolean {
  return row.type === "node" || row.type === "error";
}

export function ObjectTreePanel() {
  const { sessions } = useConnections();
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set());
  const [children, setChildren] = useState<ReadonlyMap<string, ChildrenState>>(new Map());
  const [selected, setSelected] = useState<string | null>(null);
  const [showSystem, setShowSystem] = useState(false);
  /** The request in flight per node id, to cancel it and to drop stale answers. */
  const pending = useRef(new Map<string, string>());
  const treeRef = useRef<HTMLDivElement>(null);
  const domId = useId();

  const rows = flattenTree(sessions, { expanded, children });
  const selectedRow = rows.find((r) => r.id === selected) ?? null;

  const cancelPending = useCallback((id: string) => {
    const requestId = pending.current.get(id);
    if (requestId) {
      pending.current.delete(id);
      void cancelRequest(requestId).catch(() => {});
    }
  }, []);

  const load = useCallback(
    async (sessionId: string, id: string, node: TreeNodeRef) => {
      cancelPending(id);
      const requestId = newRequestId();
      pending.current.set(id, requestId);
      setChildren((current) => {
        const next = new Map(current);
        next.set(id, { status: "loading", nodes: current.get(id)?.nodes ?? [] });
        return next;
      });
      let result: ChildrenState;
      try {
        const nodes = await treeChildren(sessionId, node, { showSystem }, requestId);
        result = { status: "ok", nodes };
      } catch (error) {
        const err = toCommandError(error);
        result = { status: "error", nodes: [], message: err.message };
      }
      // A newer request (refresh) or a collapse superseded this one.
      if (pending.current.get(id) !== requestId) return;
      pending.current.delete(id);
      setChildren((current) => {
        const next = new Map(current);
        next.set(id, result);
        return next;
      });
    },
    [cancelPending, showSystem],
  );

  // Expanded nodes without children (first expansion, refresh, show-system) are loaded.
  useEffect(() => {
    for (const row of rows) {
      if (row.type === "node" && row.expanded && !children.has(row.id)) {
        void load(row.sessionId, row.id, row.ref);
      }
    }
  });

  // Forget the nodes of connections that were closed.
  useEffect(() => {
    const open = new Set(sessions.map((s) => s.id));
    const isOpen = (id: string) => open.has(id.slice(0, id.indexOf("::")));
    for (const id of [...pending.current.keys()]) if (!isOpen(id)) cancelPending(id);
    setChildren((current) => {
      const stale = [...current.keys()].filter((id) => !isOpen(id));
      if (stale.length === 0) return current;
      const next = new Map(current);
      for (const id of stale) next.delete(id);
      return next;
    });
  }, [sessions, cancelPending]);

  // Leaving the panel cancels whatever is still loading.
  useEffect(
    () => () => {
      for (const id of [...pending.current.keys()]) cancelPending(id);
    },
    [cancelPending],
  );

  function setOpen(row: NodeRow, open: boolean) {
    if (!row.expandable || open === row.expanded) return;
    setExpanded((current) => {
      const next = new Set(current);
      if (open) next.add(row.id);
      else next.delete(row.id);
      return next;
    });
    if (open) {
      // A failed load is retried on the next expansion.
      if (children.get(row.id)?.status === "error") dropChildren([row.id]);
    } else if (pending.current.has(row.id)) {
      cancelPending(row.id);
      dropChildren([row.id]);
    }
  }

  function dropChildren(ids: string[]) {
    for (const id of ids) cancelPending(id);
    setChildren((current) => {
      const next = new Map(current);
      for (const id of ids) next.delete(id);
      return next;
    });
  }

  function refresh() {
    const row = selectedRow;
    if (row?.type === "node" && row.expandable) {
      dropChildren([row.id, ...cachedDescendants(row.sessionId, row.id, children)]);
      if (!row.expanded) setOpen(row, true);
    } else if (row?.type === "error") {
      dropChildren([row.parentId]);
    } else if (row?.type === "node" && row.parentId) {
      const parent = row.parentId;
      dropChildren([parent, ...cachedDescendants(row.sessionId, parent, children)]);
    } else {
      dropChildren([...children.keys()]);
    }
  }

  function toggleSystem() {
    setShowSystem((v) => !v);
    // Everything is listed again with the new filter.
    dropChildren([...children.keys()]);
  }

  function newQuery() {
    const row = selectedRow;
    const sessionId = row?.type === "node" ? row.sessionId : sessions[0]?.id;
    const database = row?.type === "node" ? databaseOf(row.ref) : undefined;
    openQueryEditor({
      ...(sessionId ? { sessionId } : {}),
      ...(database ? { database } : {}),
    });
  }

  function select(id: string) {
    setSelected(id);
    document.getElementById(`${domId}-${id}`)?.scrollIntoView?.({ block: "nearest" });
  }

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    const items = rows.filter(isNavigable);
    if (items.length === 0) return;
    const index = items.findIndex((r) => r.id === selected);
    const row = items[index];
    let target: Row | undefined;
    switch (event.key) {
      case "ArrowDown":
        target = items[Math.min(index + 1, items.length - 1)];
        break;
      case "ArrowUp":
        target = items[index < 0 ? 0 : Math.max(index - 1, 0)];
        break;
      case "Home":
        target = items[0];
        break;
      case "End":
        target = items[items.length - 1];
        break;
      case "ArrowRight":
        if (row?.type === "node" && row.expandable) {
          if (!row.expanded) setOpen(row, true);
          else target = items[index + 1];
        }
        break;
      case "ArrowLeft":
        if (row?.type === "node" && row.expanded) setOpen(row, false);
        else if (row?.parentId) target = items.find((r) => r.id === row.parentId);
        break;
      case "Enter":
      case " ":
        if (row?.type === "node") setOpen(row, !row.expanded);
        else if (row?.type === "error") dropChildren([row.parentId]);
        break;
      case "F5":
        refresh();
        break;
      default:
        return;
    }
    event.preventDefault();
    if (target) select(target.id);
  }

  return (
    <section className="panel panel--tree" aria-label="Objects">
      <div className="toolbar" role="toolbar" aria-label="Object tree actions">
        <button
          type="button"
          className="button"
          onClick={refresh}
          disabled={sessions.length === 0}
          title="Refresh (F5)"
        >
          Refresh
        </button>
        <button
          type="button"
          className="button"
          onClick={newQuery}
          disabled={sessions.length === 0}
        >
          New query
        </button>
        <label className="checkbox tree__system">
          <input type="checkbox" checked={showSystem} onChange={toggleSystem} />
          System objects
        </label>
      </div>
      {sessions.length === 0 ? (
        <div className="empty-state">
          <p className="empty-state__title">No open connections</p>
          <p className="empty-state__hint">Connect to a database to browse its objects.</p>
        </div>
      ) : (
        <div
          ref={treeRef}
          className="tree"
          role="tree"
          aria-label="Database objects"
          tabIndex={0}
          aria-activedescendant={selectedRow ? `${domId}-${selectedRow.id}` : undefined}
          onKeyDown={onKeyDown}
        >
          {rows.map((row) => {
            const indent = { paddingLeft: `${(row.level - 1) * 14 + 4}px` };
            if (row.type !== "node") {
              const text =
                row.type === "loading"
                  ? "Loading…"
                  : row.type === "empty"
                    ? "Empty"
                    : `Could not load: ${row.message}`;
              return (
                // biome-ignore lint/a11y/useKeyWithClickEvents: the tree handles the keyboard.
                <div
                  key={row.id}
                  id={`${domId}-${row.id}`}
                  role="treeitem"
                  aria-level={row.level}
                  aria-selected={row.id === selected}
                  tabIndex={-1}
                  className={`tree__row tree__row--${row.type}`}
                  style={indent}
                  onClick={() => row.type === "error" && setSelected(row.id)}
                >
                  <span className={`tree-icon tree-icon--${row.type}`} aria-hidden="true" />
                  <span className="tree__label">{text}</span>
                  {row.type === "error" ? (
                    <button
                      type="button"
                      className="button button--small"
                      tabIndex={-1}
                      onClick={() => dropChildren([row.parentId])}
                    >
                      Retry
                    </button>
                  ) : null}
                </div>
              );
            }
            return (
              // biome-ignore lint/a11y/useKeyWithClickEvents: the tree handles the keyboard.
              <div
                key={row.id}
                id={`${domId}-${row.id}`}
                role="treeitem"
                aria-level={row.level}
                aria-posinset={row.posInSet}
                aria-setsize={row.setSize}
                aria-selected={row.id === selected}
                {...(row.expandable ? { "aria-expanded": row.expanded } : {})}
                tabIndex={-1}
                title={row.comment}
                className={`tree__row${row.system ? " tree__row--system" : ""}`}
                style={indent}
                onClick={() => {
                  setSelected(row.id);
                  treeRef.current?.focus();
                }}
                onDoubleClick={() => setOpen(row, !row.expanded)}
              >
                <span
                  className={`tree__twisty${row.expandable ? "" : " tree__twisty--leaf"}${row.expanded ? " tree__twisty--open" : ""}`}
                  aria-hidden="true"
                  onClick={(e) => {
                    e.stopPropagation();
                    setSelected(row.id);
                    setOpen(row, !row.expanded);
                  }}
                />
                <span className={`tree-icon tree-icon--${row.icon}`} aria-hidden="true" />
                <span className="tree__label">{row.label}</span>
                {row.detail ? <span className="tree__detail">{row.detail}</span> : null}
              </div>
            );
          })}
        </div>
      )}
    </section>
  );
}
