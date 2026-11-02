import type { LucideIcon } from "lucide-react";
import { Fragment, type KeyboardEvent, useEffect, useRef } from "react";
import { createPortal } from "react-dom";
import { ICON_SM } from "./icons";

export interface MenuItem {
  label: string;
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
}

export interface ContextMenuProps {
  /** Viewport position of the menu's top-left corner. */
  x: number;
  y: number;
  label: string;
  items: MenuItem[];
  onClose: () => void;
}

const ITEM_HEIGHT = 28;
const SEPARATOR_HEIGHT = 9;
const MENU_WIDTH = 240;

/**
 * A context menu: `role="menu"` at a viewport position, focus on the first item, arrows /
 * Home / End move, Enter or Space activates, Escape, Tab or a click elsewhere closes it (focus
 * goes back to the element that opened it).
 */
export function ContextMenu({ x, y, label, items, onClose }: ContextMenuProps) {
  const ref = useRef<HTMLDivElement>(null);
  const closeRef = useRef(onClose);
  closeRef.current = onClose;

  useEffect(() => {
    const opener = document.activeElement as HTMLElement | null;
    ref.current
      ?.querySelector<HTMLElement>('[role="menuitem"]:not([aria-disabled="true"])')
      ?.focus();
    const onDown = (event: MouseEvent) => {
      if (!ref.current?.contains(event.target as Node)) closeRef.current();
    };
    document.addEventListener("mousedown", onDown);
    return () => {
      document.removeEventListener("mousedown", onDown);
      opener?.focus?.();
    };
  }, []);

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    const all = Array.from(
      ref.current?.querySelectorAll<HTMLElement>('[role="menuitem"]:not([aria-disabled="true"])') ??
        [],
    );
    const index = all.indexOf(document.activeElement as HTMLElement);
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
    next?.focus();
  }

  // Kept inside the viewport.
  const separators = items.filter((item, index) => index > 0 && item.separatorBefore).length;
  const height = items.length * ITEM_HEIGHT + separators * SEPARATOR_HEIGHT + 10;
  const left = Math.max(0, Math.min(x, window.innerWidth - MENU_WIDTH));
  const top = Math.max(0, Math.min(y, window.innerHeight - height));

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
        return (
          <Fragment key={item.label}>
            {item.separatorBefore && index > 0 ? <hr className="context-menu__separator" /> : null}
            <button
              type="button"
              role="menuitem"
              tabIndex={-1}
              className={`context-menu__item${item.danger ? " context-menu__item--danger" : ""}`}
              aria-disabled={item.disabled ? true : undefined}
              aria-keyshortcuts={item.shortcut}
              onClick={() => {
                if (item.disabled) return;
                onClose();
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
            </button>
          </Fragment>
        );
      })}
    </div>,
    document.body,
  );
}
