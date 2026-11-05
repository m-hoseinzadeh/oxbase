import { ChevronRight, type LucideIcon } from "lucide-react";
import { Fragment, type KeyboardEvent, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { ICON_SM } from "./icons";

export interface MenuItem {
  label: string;
  /** Ignored for an item with a `submenu`, which opens it instead. */
  onSelect: () => void;
  disabled?: boolean;
  /** A Lucide icon shown before the label. */
  icon?: LucideIcon;
  /** A keyboard shortcut shown after the label (e.g. "F5"); also exposed as aria-keyshortcuts. */
  shortcut?: string;
  /** A destructive action, shown in the danger colour. */
  danger?: boolean;
  /** Draw a separator line above this item. */
  separatorBefore?: boolean;
  /** Items of a submenu, opened to the right (hover, click, Enter or ArrowRight). */
  submenu?: MenuItem[];
}

export interface ContextMenuProps {
  /** Viewport position of the menu's top-left corner. */
  x: number;
  y: number;
  label: string;
  items: MenuItem[];
  onClose: () => void;
  /** A submenu: closes the whole chain of menus after an item ran (internal). */
  closeAll?: () => void;
}

const ITEM_HEIGHT = 28;
const SEPARATOR_HEIGHT = 9;
const MENU_WIDTH = 280;

/** No-op `onSelect` for items that only open a submenu. */
export const OPENS_SUBMENU = () => {};

/**
 * A context menu: `role="menu"` at a viewport position, focus on the first item, arrows /
 * Home / End move, Enter or Space activates, ArrowRight opens a submenu and ArrowLeft closes
 * it, Escape, Tab or a click elsewhere closes it (focus goes back to the element that opened
 * it).
 */
export function ContextMenu({ x, y, label, items, onClose, closeAll }: ContextMenuProps) {
  const ref = useRef<HTMLDivElement>(null);
  const closeRef = useRef(onClose);
  closeRef.current = onClose;
  const [open, setOpen] = useState<{ index: number; x: number; y: number } | null>(null);
  const isSubmenu = closeAll !== undefined;
  const finish = closeAll ?? onClose;

  useEffect(() => {
    const opener = document.activeElement as HTMLElement | null;
    ref.current
      ?.querySelector<HTMLElement>('[role="menuitem"]:not([aria-disabled="true"])')
      ?.focus();
    const onDown = (event: MouseEvent) => {
      // Clicks in this menu's submenus (other `.context-menu`s) are inside too.
      const target = event.target as Element | null;
      if (target?.closest?.(".context-menu")) return;
      if (!ref.current?.contains(target)) closeRef.current();
    };
    // A submenu is closed with the menu that opened it.
    if (!isSubmenu) document.addEventListener("mousedown", onDown);
    return () => {
      document.removeEventListener("mousedown", onDown);
      opener?.focus?.();
    };
  }, [isSubmenu]);

  function openSubmenu(index: number, button: HTMLElement) {
    const r = button.getBoundingClientRect();
    setOpen({ index, x: r.right - 4, y: r.top - 4 });
  }

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    const all = Array.from(
      ref.current?.querySelectorAll<HTMLElement>('[role="menuitem"]:not([aria-disabled="true"])') ??
        [],
    );
    const index = all.indexOf(document.activeElement as HTMLElement);
    // Keys pressed in an open submenu (a portal) bubble here through React; it handles them.
    if (index < 0 && open) return;
    let next: HTMLElement | undefined;
    switch (event.key) {
      case "ArrowDown":
        next = all[(index + 1) % all.length];
        break;
      case "ArrowUp":
        next = all[(index - 1 + all.length) % all.length];
        break;
      case "Home":
        next = all[0];
        break;
      case "End":
        next = all[all.length - 1];
        break;
      case "ArrowRight": {
        const focused = all[index];
        const at = focused ? Number(focused.dataset.index) : -1;
        if (focused && items[at]?.submenu) {
          event.preventDefault();
          event.stopPropagation();
          openSubmenu(at, focused);
        }
        return;
      }
      case "ArrowLeft":
        if (!isSubmenu) return;
        event.preventDefault();
        event.stopPropagation();
        onClose();
        return;
      case "Escape":
      case "Tab":
        event.preventDefault();
        event.stopPropagation();
        onClose();
        return;
      default:
        return;
    }
    event.preventDefault();
    event.stopPropagation();
    setOpen(null);
    next?.focus();
  }

  // Kept inside the viewport.
  const separators = items.filter((item, index) => index > 0 && item.separatorBefore).length;
  const height = items.length * ITEM_HEIGHT + separators * SEPARATOR_HEIGHT + 10;
  const left = Math.max(0, Math.min(x, window.innerWidth - MENU_WIDTH));
  const top = Math.max(0, Math.min(y, window.innerHeight - height));
  const sub = open ? items[open.index] : undefined;

  return createPortal(
    <div
      ref={ref}
      className="context-menu"
      role="menu"
      aria-label={label}
      style={{ left, top }}
      onKeyDown={onKeyDown}
    >
      {items.map((item, index) => {
        const Icon = item.icon;
        const hasSubmenu = item.submenu !== undefined;
        return (
          <Fragment key={item.label}>
            {item.separatorBefore && index > 0 ? <hr className="context-menu__separator" /> : null}
            <button
              type="button"
              role="menuitem"
              tabIndex={-1}
              data-index={index}
              className={`context-menu__item${item.danger ? " context-menu__item--danger" : ""}`}
              aria-disabled={item.disabled ? true : undefined}
              aria-keyshortcuts={item.shortcut}
              aria-haspopup={hasSubmenu ? "menu" : undefined}
              aria-expanded={hasSubmenu ? open?.index === index : undefined}
              onMouseEnter={(event) => {
                if (item.disabled) return;
                if (hasSubmenu) openSubmenu(index, event.currentTarget);
                else setOpen(null);
              }}
              onClick={(event) => {
                if (item.disabled) return;
                if (hasSubmenu) {
                  openSubmenu(index, event.currentTarget);
                  return;
                }
                finish();
                item.onSelect();
              }}
            >
              <span className="context-menu__icon" aria-hidden="true">
                {Icon ? <Icon {...ICON_SM} /> : null}
              </span>
              <span className="context-menu__label">{item.label}</span>
              {item.shortcut ? (
                <span className="kbd context-menu__shortcut" aria-hidden="true">
                  {item.shortcut}
                </span>
              ) : null}
              {hasSubmenu ? (
                <span className="context-menu__chevron" aria-hidden="true">
                  <ChevronRight {...ICON_SM} />
                </span>
              ) : null}
            </button>
          </Fragment>
        );
      })}
      {open && sub?.submenu ? (
        <ContextMenu
          x={open.x}
          y={open.y}
          label={sub.label}
          items={sub.submenu}
          onClose={() => setOpen(null)}
          closeAll={finish}
        />
      ) : null}
    </div>,
    document.body,
  );
}
