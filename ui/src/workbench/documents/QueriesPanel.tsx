/**
 * 1.3: the Queries panel. Saved queries of every saved connection (connected or not), grouped
 * by connection: double-click or Enter opens one in a query tab on its connection, F2 renames,
 * Delete deletes (after asking). Saved queries are `.sql` files kept by the app
 * (docs/files.md); a query tab saves a new one with Ctrl+S.
 */
import { FileCode2, FolderOpen, Pencil, RefreshCw, Trash2 } from "lucide-react";
import {
  Fragment,
  type KeyboardEvent,
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
} from "react";
import { type SavedQueryItem, savedQueryDelete, savedQueryList, savedQueryRename } from "../../api";
import { ContextMenu, type MenuItem } from "../../components/ContextMenu";
import { ICON, ICON_LG, ICON_SM } from "../../components/icons";
import { toCommandError } from "../../connections/api";
import { EngineBadge } from "../../connections/EngineBadge";
import { useConnections } from "../../connections/store";
import type { ConnectionProfile } from "../../connections/types";
import { setPanelTitle } from "../layout";
import { notifySavedQueriesChanged, onSavedQueriesChanged, openDocumentById } from "./actions";
import { askConfirm, askName, showFileError } from "./dialogs";
import { detachDocument, updateDocument } from "./tabs";

interface Group {
  profileId: string;
  profile: ConnectionProfile | null;
  queries: SavedQueryItem[];
}

/** Saved queries by connection, in the Connections panel's order; unknown connections last. */
export function groupQueries(items: SavedQueryItem[], profiles: ConnectionProfile[]): Group[] {
  const byProfile = new Map<string, SavedQueryItem[]>();
  for (const item of items) {
    const list = byProfile.get(item.profileId) ?? [];
    list.push(item);
    byProfile.set(item.profileId, list);
  }
  const groups: Group[] = [];
  for (const profile of profiles) {
    const queries = byProfile.get(profile.id);
    if (queries) groups.push({ profileId: profile.id, profile, queries });
    byProfile.delete(profile.id);
  }
  for (const [profileId, queries] of byProfile) groups.push({ profileId, profile: null, queries });
  return groups;
}

export function QueriesPanel() {
  const { profiles } = useConnections();
  const [items, setItems] = useState<SavedQueryItem[]>([]);
  const [status, setStatus] = useState<"loading" | "ok" | "error">("loading");
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [menu, setMenu] = useState<{ item: SavedQueryItem; x: number; y: number } | null>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const domId = useId();

  const load = useCallback(async () => {
    try {
      setItems(await savedQueryList());
      setStatus("ok");
      setError(null);
    } catch (e) {
      setStatus("error");
      setError(toCommandError(e).message);
    }
  }, []);

  useEffect(() => {
    void load();
    return onSavedQueriesChanged(() => void load());
  }, [load]);

  const groups = groupQueries(items, profiles);
  const flat = groups.flatMap((g) => g.queries);
  const selectedItem = flat.find((q) => q.document.id === selected) ?? null;

  function open(item: SavedQueryItem) {
    void openDocumentById(item.document.id, item.document.name);
  }

  async function rename(item: SavedQueryItem) {
    await askName({
      title: "Rename query",
      label: "Query name",
      initial: item.document.name,
      action: "Rename",
      submit: async (name) => {
        try {
          const info = await savedQueryRename(item.document.id, name);
          for (const panelId of updateDocument(info)) setPanelTitle(panelId, info.name);
          notifySavedQueriesChanged();
          return null;
        } catch (e) {
          return toCommandError(e).message;
        }
      },
    });
    listRef.current?.focus();
  }

  async function remove(item: SavedQueryItem) {
    const ok = await askConfirm(
      "Delete query?",
      `Delete the saved query ${item.document.name}? Its file is removed. An open tab of it keeps its text, unsaved.`,
      "Delete",
    );
    if (!ok) return;
    try {
      await savedQueryDelete(item.document.id);
      detachDocument(item.document.id);
      notifySavedQueriesChanged();
    } catch (e) {
      await showFileError("Could not delete the query", toCommandError(e).message);
    }
    listRef.current?.focus();
  }

  function menuItems(item: SavedQueryItem): MenuItem[] {
    return [
      { label: "Open", icon: FolderOpen, shortcut: "Enter", onSelect: () => open(item) },
      { label: "Rename…", icon: Pencil, shortcut: "F2", onSelect: () => void rename(item) },
      {
        label: "Delete…",
        icon: Trash2,
        shortcut: "Delete",
        danger: true,
        separatorBefore: true,
        onSelect: () => void remove(item),
      },
    ];
  }

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    const index = flat.findIndex((q) => q.document.id === selected);
    let next: SavedQueryItem | undefined;
    switch (event.key) {
      case "ArrowDown":
        next = flat[Math.min(index + 1, flat.length - 1)];
        break;
      case "ArrowUp":
        next = flat[index < 0 ? 0 : Math.max(index - 1, 0)];
        break;
      case "Home":
        next = flat[0];
        break;
      case "End":
        next = flat[flat.length - 1];
        break;
      case "Enter":
        if (selectedItem) open(selectedItem);
        break;
      case "F2":
        if (selectedItem) void rename(selectedItem);
        break;
      case "Delete":
        if (selectedItem) void remove(selectedItem);
        break;
      case "F5":
        void load();
        break;
      default:
        return;
    }
    event.preventDefault();
    if (next) setSelected(next.document.id);
  }

  return (
    <section className="panel panel--tree" aria-label="Saved queries">
      <div className="toolbar tree__toolbar" role="toolbar" aria-label="Saved query actions">
        <button
          type="button"
          className="icon-button"
          onClick={() => void load()}
          aria-label="Refresh"
          title="Refresh (F5)"
        >
          <RefreshCw {...ICON} />
        </button>
        <button
          type="button"
          className="icon-button"
          disabled={!selectedItem}
          onClick={() => selectedItem && void rename(selectedItem)}
          aria-label="Rename"
          title="Rename (F2)"
        >
          <Pencil {...ICON} />
        </button>
        <button
          type="button"
          className="icon-button"
          disabled={!selectedItem}
          onClick={() => selectedItem && void remove(selectedItem)}
          aria-label="Delete"
          title="Delete (Delete)"
        >
          <Trash2 {...ICON} />
        </button>
      </div>
      {status === "error" ? (
        <p className="callout callout--danger" role="alert">
          <span className="callout__text">Could not list the saved queries: {error}</span>
        </p>
      ) : flat.length === 0 ? (
        <div className="empty-state">
          <span className="empty-state__icon" aria-hidden="true">
            <FileCode2 {...ICON_LG} />
          </span>
          <p className="empty-state__title">
            {status === "loading" ? "Loading…" : "No saved queries"}
          </p>
          {status === "loading" ? null : (
            <p className="empty-state__hint">
              In a query tab, press Ctrl+S to save the query with its connection.
            </p>
          )}
        </div>
      ) : (
        <div
          ref={listRef}
          className="tree"
          role="tree"
          aria-label="Saved queries"
          tabIndex={0}
          data-testid="saved-queries"
          aria-activedescendant={selectedItem ? `${domId}-${selectedItem.document.id}` : undefined}
          onKeyDown={onKeyDown}
        >
          {groups.map((group) => (
            <Fragment key={group.profileId}>
              <div className="tree__row tree__row--group" aria-hidden="true">
                {group.profile ? <EngineBadge engine={group.profile.engine} size="sm" /> : null}
                <span className="tree__label">{group.profile?.name ?? "Deleted connection"}</span>
                <span className="tree__detail">{group.queries.length}</span>
              </div>
              {group.queries.map((item) => (
                // biome-ignore lint/a11y/useKeyWithClickEvents: the tree handles the keyboard.
                <div
                  key={item.document.id}
                  id={`${domId}-${item.document.id}`}
                  role="treeitem"
                  aria-level={2}
                  aria-selected={item.document.id === selected}
                  tabIndex={-1}
                  className="tree__row"
                  style={{ paddingLeft: "20px" }}
                  data-testid="saved-query"
                  title={item.database ? `Runs in ${item.database}` : undefined}
                  onClick={() => {
                    setSelected(item.document.id);
                    listRef.current?.focus();
                  }}
                  onDoubleClick={() => open(item)}
                  onContextMenu={(e) => {
                    e.preventDefault();
                    setSelected(item.document.id);
                    setMenu({ item, x: e.clientX, y: e.clientY });
                  }}
                >
                  <span className="tree-icon tree-icon--routine" aria-hidden="true">
                    <FileCode2 {...ICON_SM} />
                  </span>
                  <span className="tree__label">{item.document.name}</span>
                  {item.database ? <span className="tree__detail">{item.database}</span> : null}
                </div>
              ))}
            </Fragment>
          ))}
        </div>
      )}
      {menu ? (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          label={menu.item.document.name}
          items={menuItems(menu.item)}
          onClose={() => setMenu(null)}
        />
      ) : null}
    </section>
  );
}
