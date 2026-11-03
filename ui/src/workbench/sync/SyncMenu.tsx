/**
 * The app bar's M8 entry points: Data transfer, a Compare menu (structure sync, data sync) and
 * a Backup menu (back up, restore, verify).
 */
import {
  Archive,
  ArchiveRestore,
  ArrowLeftRight,
  ChevronDown,
  DatabaseBackup,
  GitCompareArrows,
  ListTree,
  Rows3,
  ShieldCheck,
} from "lucide-react";
import { useRef, useState } from "react";
import { ContextMenu, type MenuItem } from "../../components/ContextMenu";
import { ICON } from "../../components/icons";
import { openDataSync, openStructureSync } from "../layout";
import { openBackup, openRestore, openTransfer } from "./store";

type Open = { menu: "compare" | "backup"; x: number; y: number } | null;

export function SyncMenu() {
  const [open, setOpen] = useState<Open>(null);
  const compareRef = useRef<HTMLButtonElement>(null);
  const backupRef = useRef<HTMLButtonElement>(null);

  function show(menu: "compare" | "backup") {
    const button = (menu === "compare" ? compareRef : backupRef).current;
    const rect = button?.getBoundingClientRect();
    setOpen({ menu, x: rect?.left ?? 0, y: rect?.bottom ?? 0 });
  }

  const items: Record<"compare" | "backup", MenuItem[]> = {
    compare: [
      { label: "Structure sync", icon: ListTree, onSelect: () => openStructureSync() },
      { label: "Data sync", icon: Rows3, onSelect: () => openDataSync() },
    ],
    backup: [
      { label: "Back up…", icon: Archive, onSelect: () => openBackup() },
      { label: "Restore…", icon: ArchiveRestore, onSelect: () => openRestore() },
      {
        label: "Verify a backup…",
        icon: ShieldCheck,
        separatorBefore: true,
        onSelect: () => openRestore({ verifyOnly: true }),
      },
    ],
  };

  return (
    <>
      <button
        type="button"
        className="button button--ghost"
        data-testid="app-transfer"
        title="Copy tables between connections and engines"
        onClick={() => openTransfer()}
      >
        <ArrowLeftRight {...ICON} />
        Transfer…
      </button>
      <button
        ref={compareRef}
        type="button"
        className="button button--ghost"
        data-testid="app-compare"
        aria-haspopup="menu"
        aria-expanded={open?.menu === "compare"}
        onClick={() => show("compare")}
      >
        <GitCompareArrows {...ICON} />
        Compare
        <ChevronDown {...ICON} />
      </button>
      <button
        ref={backupRef}
        type="button"
        className="button button--ghost"
        data-testid="app-backup"
        aria-haspopup="menu"
        aria-expanded={open?.menu === "backup"}
        onClick={() => show("backup")}
      >
        <DatabaseBackup {...ICON} />
        Backup
        <ChevronDown {...ICON} />
      </button>
      {open ? (
        <ContextMenu
          x={open.x}
          y={open.y}
          label={open.menu === "compare" ? "Compare" : "Backup"}
          items={items[open.menu]}
          onClose={() => setOpen(null)}
        />
      ) : null}
    </>
  );
}
