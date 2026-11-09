/**
 * 1.3: the app bar's Open button (Ctrl+O) and its menu of recent files.
 */
import { ChevronDown, FileClock, FolderOpen, X } from "lucide-react";
import { useRef, useState } from "react";
import type { DocumentInfo } from "../../api";
import { ContextMenu, type MenuItem } from "../../components/ContextMenu";
import { ICON } from "../../components/icons";
import { clearRecentFiles, openDocumentById, openFileDialog, recentFiles } from "./actions";

export function OpenMenu() {
  const [menu, setMenu] = useState<{ x: number; y: number; recent: DocumentInfo[] } | null>(null);
  const more = useRef<HTMLButtonElement>(null);

  async function show() {
    const rect = more.current?.getBoundingClientRect();
    let recent: DocumentInfo[] = [];
    try {
      recent = await recentFiles();
    } catch {
      // No core: only "Open file…" is offered.
    }
    setMenu({ x: rect?.left ?? 0, y: rect?.bottom ?? 0, recent });
  }

  const items: MenuItem[] = menu
    ? [
        {
          label: "Open file…",
          icon: FolderOpen,
          shortcut: "Ctrl+O",
          onSelect: () => void openFileDialog(),
        },
        ...menu.recent.map(
          (doc, i): MenuItem => ({
            label: `${doc.name} — ${doc.location}`,
            icon: FileClock,
            ...(i === 0 ? { separatorBefore: true } : {}),
            onSelect: () => void openDocumentById(doc.id, doc.name),
          }),
        ),
        {
          label: menu.recent.length > 0 ? "Clear recent files" : "No recent files",
          icon: X,
          separatorBefore: true,
          disabled: menu.recent.length === 0,
          onSelect: () => void clearRecentFiles().catch(() => {}),
        },
      ]
    : [];

  return (
    <span className="split-button">
      <button
        type="button"
        className="button button--ghost split-button__main"
        title="Open a .sql file in a query tab (Ctrl+O)"
        onClick={() => void openFileDialog()}
      >
        <FolderOpen {...ICON} />
        Open…
      </button>
      <button
        ref={more}
        type="button"
        className="button button--ghost split-button__more"
        data-testid="open-recent"
        aria-label="Recent files"
        title="Recent files"
        aria-haspopup="menu"
        aria-expanded={menu !== null}
        onClick={() => void show()}
      >
        <ChevronDown {...ICON} />
      </button>
      {menu ? (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          label="Open"
          items={items}
          onClose={() => setMenu(null)}
        />
      ) : null}
    </span>
  );
}
