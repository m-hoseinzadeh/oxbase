/**
 * The object tree: every open connection, with its databases, schemas, per-kind folders and
 * objects below it. Children are listed on demand (`tree_children`) when a node is first
 * expanded; a collapsed node's pending request is cancelled. Keyboard: arrows move and
 * expand/collapse, Home/End jump, Enter toggles (or retries a failed load), F5 refreshes.
 *
 * M7: a MongoDB collection opens in a collection tab and "New query" opens the mongo shell; a
 * Redis key opens in the key browser, and Redis rows offer the key browser, the console and
 * Pub/Sub. SQL-only actions (design, new table, export) are not offered for them.
 */
import {
  Archive,
  ArchiveRestore,
  ArrowLeftRight,
  ChevronRight,
  CircleAlert,
  FileInput,
  FileOutput,
  KeyRound,
  ListTree,
  Loader2,
  PencilRuler,
  Radio,
  RefreshCw,
  RotateCw,
  Rows3,
  SquarePen,
  SquareTerminal,
  Table2,
  TableProperties,
  Unplug,
} from "lucide-react";
import { type KeyboardEvent, useCallback, useEffect, useId, useRef, useState } from "react";
import { type ObjectRef, type TreeNodeRef, treeChildren } from "../../api";
import { ContextMenu, type MenuItem } from "../../components/ContextMenu";
import { ICON, ICON_LG, ICON_SM } from "../../components/icons";
import { cancelRequest, newRequestId, toCommandError } from "../../connections/api";
import { useConnections } from "../../connections/store";
import {
  openCollection,
  openConsole,
  openDataSync,
  openKeyBrowser,
  openPubSub,
  openQueryEditor,
  openStructureSync,
  openTableData,
  openTableDesigner,
} from "../layout";
import { type EngineFamily, engineFamily } from "../nosql/engines";
import { prefixPattern } from "../nosql/keys/keyModel";
import { openBackup, openRestore, openTransfer } from "../sync/store";
import { openExport, openImport } from "../transfer/store";
import { TreeIcon } from "./TreeIcon";
import {
  type ChildrenState,
  cachedDescendants,
  databaseOf,
  flattenTree,
  keyBytesOf,
  type Row,
} from "./treeModel";

type NodeRow = Extract<Row, { type: "node" }>;

/** Kinds whose rows open in a table-data tab. */
const DATA_KINDS: ReadonlySet<string> = new Set(["table", "view", "mat_view"]);

/** The object a row's data or design opens, if it is a table (or view, for data). */
function dataObject(row: Row | null): ObjectRef | null {
  if (row?.type !== "node" || row.ref.type !== "object") return null;
  return DATA_KINDS.has(row.ref.object.kind) ? row.ref.object : null;
}

/** Where a new table goes when `row` is selected: its schema, or its database. */
export function newTableTarget(row: Row | null): ObjectRef | null {
  if (row?.type !== "node") return null;
  const ref = row.ref;
  const object = ref.type === "object" ? ref.object : ref.type === "folder" ? ref.parent : null;
  if (!object) return null;
  if (object.kind === "schema") {
    return {
      kind: "table",
      ...(object.catalog ? { catalog: object.catalog } : {}),
      schema: object.name,
      name: "",
    };
  }
  if (object.kind === "database") return { kind: "table", catalog: object.name, name: "" };
  if (object.kind === "table" || object.kind === "view") {
    return {
      kind: "table",
      ...(object.catalog ? { catalog: object.catalog } : {}),
      ...(object.schema ? { schema: object.schema } : {}),
      name: "",
    };
  }
  return null;
}

/** Where an import from `row` goes: its database or schema (a new table) or its table. */
export function importTarget(row: Row | null): ObjectRef | null | undefined {
  if (row?.type !== "node") return undefined;
  const ref = row.ref;
  if (ref.type === "connection") return null;
  if (ref.type === "folder") {
    return ref.kind === "table" && (ref.parent.kind === "database" || ref.parent.kind === "schema")
      ? ref.parent
      : undefined;
  }
  const kind = ref.object.kind;
  return kind === "database" || kind === "schema" || kind === "table" ? ref.object : undefined;
}

/** The Redis key a row stands for, if it is a key node. */
function keyObject(row: Row | null): ObjectRef | null {
  if (row?.type !== "node" || row.ref.type !== "object") return null;
  return row.ref.object.kind === "key" ? row.ref.object : null;
}

/** The namespace prefix (`m7:ns:`) of a Redis namespace row. */
function namespaceOf(row: Row): string | undefined {
  if (row.type !== "node" || row.ref.type !== "object") return undefined;
  return row.ref.object.kind === "schema" ? row.ref.object.name : undefined;
}

/** The database or schema a row stands for (M8 transfer, sync, backup), if it is one. */
export function containerOf(row: Row | null): ObjectRef | null {
  if (row?.type !== "node" || row.ref.type !== "object") return null;
  const kind = row.ref.object.kind;
  return kind === "database" || kind === "schema" ? row.ref.object : null;
}

/** The database or schema a table or view lives in. */
export function parentContainer(object: ObjectRef): ObjectRef | null {
  if (object.schema) {
    return {
      kind: "schema",
      ...(object.catalog ? { catalog: object.catalog } : {}),
      name: object.schema,
    };
  }
  return object.catalog ? { kind: "database", name: object.catalog } : null;
}

function isNavigable(row: Row): boolean {
  return row.type === "node" || row.type === "error";
}

export function ObjectTreePanel() {
  const { sessions } = useConnections();
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set());
  const [children, setChildren] = useState<ReadonlyMap<string, ChildrenState>>(new Map());
  const [selected, setSelected] = useState<string | null>(null);
  const [showSystem, setShowSystem] = useState(false);
  const [menu, setMenu] = useState<{ rowId: string; x: number; y: number } | null>(null);
  /** The request in flight per node id, to cancel it and to drop stale answers. */
  const pending = useRef(new Map<string, string>());
  const treeRef = useRef<HTMLDivElement>(null);
  const domId = useId();

  const rows = flattenTree(sessions, { expanded, children });
  const selectedRow = rows.find((r) => r.id === selected) ?? null;
  const familyOf = (row: Row | null): EngineFamily =>
    engineFamily(
      row?.type === "node" ? sessions.find((s) => s.id === row.sessionId)?.engine : undefined,
    );
  /** Where a new table goes (SQL engines only). */
  const tableTargetOf = (row: Row | null) => (familyOf(row) === "sql" ? newTableTarget(row) : null);
  /** Where an import goes (not into a key-value store). */
  const importTargetOf = (row: Row | null) =>
    familyOf(row) === "keyvalue" ? undefined : importTarget(row);

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

  /** A query editor for the row's database: the mongo shell, the Redis console or SQL. */
  function queryFor(row: Row | null) {
    const sessionId = row?.type === "node" ? row.sessionId : sessions[0]?.id;
    const database = row?.type === "node" ? databaseOf(row.ref) : undefined;
    const family = row?.type === "node" ? familyOf(row) : engineFamily(sessions[0]?.engine);
    if (family === "keyvalue" && sessionId) {
      openConsole({ sessionId, ...(database ? { database } : {}) });
      return;
    }
    openQueryEditor({
      ...(sessionId ? { sessionId } : {}),
      ...(database ? { database } : {}),
      ...(family === "document" ? { title: `Shell${database ? ` ${database}` : ""}` } : {}),
    });
  }

  function newQuery() {
    queryFor(selectedRow);
  }

  function openKeys(row: Row, withKey: boolean) {
    if (row.type !== "node") return;
    const database = databaseOf(row.ref);
    const key = keyObject(row);
    const namespace = namespaceOf(row);
    openKeyBrowser({
      sessionId: row.sessionId,
      ...(database ? { database } : {}),
      ...(namespace ? { pattern: prefixPattern(namespace) } : {}),
      ...(withKey && key ? { openKey: keyBytesOf(key, row.extras) } : {}),
    });
  }

  /** Whether Enter / double-click / Open has something to open for the row. */
  function canOpen(row: Row | null): boolean {
    return dataObject(row) !== null || keyObject(row) !== null;
  }

  function openData(row: Row | null = selectedRow) {
    if (row?.type !== "node") return;
    if (keyObject(row)) {
      openKeys(row, true);
      return;
    }
    const object = dataObject(row);
    if (!object) return;
    if (familyOf(row) === "document") {
      openCollection({ sessionId: row.sessionId, collection: object });
    } else {
      openTableData({ sessionId: row.sessionId, table: object });
    }
  }

  function design() {
    const row = selectedRow;
    const object = dataObject(row);
    if (row?.type === "node" && object?.kind === "table" && familyOf(row) === "sql") {
      openTableDesigner({ sessionId: row.sessionId, table: object });
    }
  }

  function newTable() {
    const row = selectedRow;
    const target = tableTargetOf(row);
    if (row?.type === "node" && target) {
      openTableDesigner({ sessionId: row.sessionId, table: target, isNew: true });
    }
  }

  function importInto(row: Row | null = selectedRow) {
    const target = importTargetOf(row);
    if (row?.type !== "node" || target === undefined) return;
    openImport({ sessionId: row.sessionId, ...(target ? { target } : {}) });
  }

  function exportRows(row: Row | null = selectedRow) {
    const object = dataObject(row);
    if (row?.type !== "node" || !object || familyOf(row) !== "sql") return;
    const engine = sessions.find((s) => s.id === row.sessionId)?.engine;
    const what = object.kind === "table" ? "Table" : "View";
    openExport({
      source: { type: "table", session: row.sessionId, query: { table: object } },
      ...(engine ? { engine } : {}),
      label: `${what} ${[object.schema ?? object.catalog, object.name].filter(Boolean).join(".")}`,
      baseName: object.name,
      offerFilter: true,
    });
  }

  function menuItems(row: Row): MenuItem[] {
    if (row.type !== "node") return [];
    const family = familyOf(row);
    if (family === "keyvalue") return keyValueItems(row);
    const items: MenuItem[] = [];
    const object = dataObject(row);
    if (object) {
      items.push({ label: "Open", icon: Table2, onSelect: () => openData(row) });
      if (object.kind === "table" && family === "sql") {
        items.push({
          label: "Design",
          icon: PencilRuler,
          onSelect: () => openTableDesigner({ sessionId: row.sessionId, table: object }),
        });
      }
    }
    const tableTarget = tableTargetOf(row);
    if (tableTarget) {
      items.push({
        label: "New table",
        icon: TableProperties,
        separatorBefore: true,
        onSelect: () =>
          openTableDesigner({ sessionId: row.sessionId, table: tableTarget, isNew: true }),
      });
    }
    items.push({
      label: family === "document" ? "Mongo shell" : "New query",
      icon: family === "document" ? SquareTerminal : SquarePen,
      separatorBefore: !tableTarget,
      onSelect: () => queryFor(row),
    });
    if (importTargetOf(row) !== undefined) {
      items.push({
        label: object ? "Import into this table…" : "Import data…",
        icon: FileInput,
        separatorBefore: true,
        onSelect: () => importInto(row),
      });
    }
    if (object && family === "sql") {
      items.push({
        label: "Export…",
        icon: FileOutput,
        separatorBefore: importTargetOf(row) === undefined,
        onSelect: () => exportRows(row),
      });
      const parent = parentContainer(object);
      items.push({
        label: "Transfer…",
        icon: ArrowLeftRight,
        onSelect: () =>
          openTransfer({
            source: { sessionId: row.sessionId, ...(parent ? { container: parent } : {}) },
            objects: [object],
          }),
      });
    }
    const container = family === "sql" ? containerOf(row) : null;
    if (container) {
      const side = { sessionId: row.sessionId, container };
      items.push(
        {
          label: "Data transfer…",
          icon: ArrowLeftRight,
          separatorBefore: true,
          onSelect: () => openTransfer({ source: side }),
        },
        {
          label: "Structure sync…",
          icon: ListTree,
          onSelect: () => openStructureSync({ source: side }),
        },
        { label: "Data sync…", icon: Rows3, onSelect: () => openDataSync({ source: side }) },
        {
          label: "Back up…",
          icon: Archive,
          separatorBefore: true,
          onSelect: () => openBackup({ source: side }),
        },
        {
          label: "Restore into…",
          icon: ArchiveRestore,
          onSelect: () => openRestore({ target: side }),
        },
      );
    }
    items.push({
      label: "Refresh",
      icon: RefreshCw,
      shortcut: "F5",
      separatorBefore: true,
      onSelect: refresh,
    });
    return items;
  }

  /** Redis rows: the key (or the key browser here), the console and Pub/Sub. */
  function keyValueItems(row: Row): MenuItem[] {
    if (row.type !== "node") return [];
    const items: MenuItem[] = [];
    const database = databaseOf(row.ref);
    if (keyObject(row)) {
      items.push({ label: "Open key", icon: KeyRound, onSelect: () => openKeys(row, true) });
    } else if (row.ref.type !== "object" || row.ref.object.kind !== "user") {
      items.push({
        label: namespaceOf(row) ? "Browse these keys" : "Key browser",
        icon: KeyRound,
        onSelect: () => openKeys(row, false),
      });
    }
    items.push({
      label: "Console",
      icon: SquareTerminal,
      separatorBefore: true,
      onSelect: () => openConsole({ sessionId: row.sessionId, ...(database ? { database } : {}) }),
    });
    items.push({
      label: "Pub/Sub",
      icon: Radio,
      onSelect: () => openPubSub({ sessionId: row.sessionId }),
    });
    items.push({
      label: "Refresh",
      icon: RefreshCw,
      shortcut: "F5",
      separatorBefore: true,
      onSelect: refresh,
    });
    return items;
  }

  function openMenu(row: Row, x: number, y: number) {
    if (row.type !== "node") return;
    setSelected(row.id);
    setMenu({ rowId: row.id, x, y });
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
        if (row && canOpen(row)) openData(row);
        else if (row?.type === "node") setOpen(row, !row.expanded);
        else if (row?.type === "error") dropChildren([row.parentId]);
        break;
      case " ":
        if (row?.type === "node") setOpen(row, !row.expanded);
        else if (row?.type === "error") dropChildren([row.parentId]);
        break;
      case "F5":
        refresh();
        break;
      case "ContextMenu":
      case "F10":
        if (event.key === "F10" && !event.shiftKey) return;
        if (row?.type === "node") {
          const rect = document.getElementById(`${domId}-${row.id}`)?.getBoundingClientRect();
          openMenu(row, (rect?.left ?? 0) + 24, rect?.bottom ?? 0);
        }
        break;
      default:
        return;
    }
    event.preventDefault();
    if (target) select(target.id);
  }

  return (
    <section className="panel panel--tree" aria-label="Objects">
      <div className="toolbar tree__toolbar" role="toolbar" aria-label="Object tree actions">
        <button
          type="button"
          className="icon-button"
          onClick={refresh}
          disabled={sessions.length === 0}
          aria-label="Refresh"
          title="Refresh (F5)"
        >
          <RefreshCw {...ICON} />
        </button>
        <button
          type="button"
          className="icon-button"
          onClick={newQuery}
          disabled={sessions.length === 0}
          aria-label="New query"
          title="New query in the selected database"
        >
          <SquarePen {...ICON} />
        </button>
        <span className="toolbar__separator" aria-hidden="true" />
        <button
          type="button"
          className="icon-button"
          onClick={() => openData()}
          disabled={!canOpen(selectedRow)}
          aria-label="Open"
          title="Open the rows (Enter or double-click)"
        >
          <Table2 {...ICON} />
        </button>
        <button
          type="button"
          className="icon-button"
          onClick={design}
          disabled={dataObject(selectedRow)?.kind !== "table" || familyOf(selectedRow) !== "sql"}
          aria-label="Design"
          title="Design the selected table"
        >
          <PencilRuler {...ICON} />
        </button>
        <button
          type="button"
          className="icon-button"
          onClick={newTable}
          disabled={tableTargetOf(selectedRow) === null}
          aria-label="New table"
          title="New table in the selected database or schema"
        >
          <TableProperties {...ICON} />
        </button>
        <button
          type="button"
          className="icon-button"
          onClick={() => importInto()}
          disabled={importTargetOf(selectedRow) === undefined}
          aria-label="Import"
          title="Import a file into the selected database, schema or table"
        >
          <FileInput {...ICON} />
        </button>
        <label className="checkbox tree__system" title="Show system databases, schemas and objects">
          <input type="checkbox" checked={showSystem} onChange={toggleSystem} />
          System objects
        </label>
      </div>
      {sessions.length === 0 ? (
        <div className="empty-state">
          <span className="empty-state__icon" aria-hidden="true">
            <Unplug {...ICON_LG} />
          </span>
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
            const indent = { paddingLeft: `${(row.level - 1) * 16 + 4}px` };
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
                  <span className="tree__twisty tree__twisty--leaf" aria-hidden="true" />
                  <span className={`tree-icon tree-icon--${row.type}`} aria-hidden="true">
                    {row.type === "loading" ? (
                      <Loader2 {...ICON_SM} className="spin" />
                    ) : row.type === "error" ? (
                      <CircleAlert {...ICON_SM} />
                    ) : null}
                  </span>
                  <span className="tree__label" title={row.type === "error" ? text : undefined}>
                    {text}
                  </span>
                  {row.type === "error" ? (
                    <button
                      type="button"
                      className="button button--small button--ghost tree__retry"
                      tabIndex={-1}
                      onClick={() => dropChildren([row.parentId])}
                    >
                      <RotateCw {...ICON_SM} />
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
                onDoubleClick={() => {
                  if (canOpen(row)) openData(row);
                  else setOpen(row, !row.expanded);
                }}
                onContextMenu={(e) => {
                  e.preventDefault();
                  treeRef.current?.focus();
                  openMenu(row, e.clientX, e.clientY);
                }}
              >
                <span
                  className={`tree__twisty${row.expandable ? "" : " tree__twisty--leaf"}${row.expanded ? " tree__twisty--open" : ""}`}
                  aria-hidden="true"
                  onClick={(e) => {
                    e.stopPropagation();
                    setSelected(row.id);
                    setOpen(row, !row.expanded);
                  }}
                >
                  {row.expandable ? <ChevronRight {...ICON_SM} /> : null}
                </span>
                <TreeIcon row={row} />
                <span className="tree__label">{row.label}</span>
                {row.detail ? <span className="tree__detail">{row.detail}</span> : null}
              </div>
            );
          })}
        </div>
      )}
      {menu
        ? (() => {
            const row = rows.find((r) => r.id === menu.rowId);
            return row ? (
              <ContextMenu
                x={menu.x}
                y={menu.y}
                label={`${row.type === "node" ? row.label : ""} actions`}
                items={menuItems(row)}
                onClose={() => setMenu(null)}
              />
            ) : null;
          })()
        : null}
    </section>
  );
}
