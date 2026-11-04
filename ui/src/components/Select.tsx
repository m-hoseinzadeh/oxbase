import { Check } from "lucide-react";
import {
  type ComponentPropsWithoutRef,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import { createPortal } from "react-dom";
import { ICON_SM } from "./icons";

/**
 * A `<select>` with a themed dropdown list.
 *
 * The native `<select>` stays the control: it keeps its label, focus, value, `change` events
 * and `role="combobox"`, so callers (and tests) use it exactly like a native one. Only the
 * popup is replaced: the native list is drawn by the OS (WebView2's is unthemed and dated, and
 * the others ignore most CSS), so opening it is intercepted and an `role="listbox"` list in the
 * design tokens is shown instead. Picking an option sets the native value and dispatches a real
 * `change` event. Arrow keys on the closed control still step through the options natively.
 */
export type SelectProps = ComponentPropsWithoutRef<"select">;

interface Item {
  value: string;
  label: string;
  disabled: boolean;
  /** The `<optgroup>` label, when the option is in one. */
  group: string | null;
}

const LIST_MAX_HEIGHT = 320;
const GAP = 4;

function readItems(select: HTMLSelectElement): Item[] {
  return Array.from(select.options).map((o) => {
    const parent = o.parentElement;
    const group = parent instanceof HTMLOptGroupElement ? parent : null;
    return {
      value: o.value,
      label: o.label || o.text,
      disabled: o.disabled || (group?.disabled ?? false),
      group: group ? group.label : null,
    };
  });
}

/** Sets the native value the way a user pick does, so React's `onChange` runs. */
function pick(select: HTMLSelectElement, value: string) {
  if (select.value === value) return;
  const setter = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value")?.set;
  if (setter) setter.call(select, value);
  else select.value = value;
  select.dispatchEvent(new Event("input", { bubbles: true }));
  select.dispatchEvent(new Event("change", { bubbles: true }));
}

interface Placement {
  left: number;
  top: number;
  minWidth: number;
  maxHeight: number;
}

function place(select: HTMLSelectElement): Placement {
  const r = select.getBoundingClientRect();
  const below = window.innerHeight - r.bottom - GAP * 2;
  const above = r.top - GAP * 2;
  const up = below < Math.min(LIST_MAX_HEIGHT, 160) && above > below;
  const maxHeight = Math.max(80, Math.min(LIST_MAX_HEIGHT, up ? above : below));
  return {
    left: Math.max(GAP, Math.min(r.left, window.innerWidth - r.width - GAP)),
    // An upward list is anchored by its bottom edge in `SelectList`.
    top: up ? r.top - GAP : r.bottom + GAP,
    minWidth: r.width,
    maxHeight: up ? -maxHeight : maxHeight,
  };
}

export function Select({ onMouseDown, onKeyDown, disabled, children, ...rest }: SelectProps) {
  const ref = useRef<HTMLSelectElement>(null);
  const listId = useId();
  const [open, setOpen] = useState<{ items: Item[]; placement: Placement } | null>(null);

  const show = useCallback(() => {
    const select = ref.current;
    if (!select || select.disabled) return;
    const items = readItems(select);
    if (items.length === 0) return;
    select.focus();
    setOpen({ items, placement: place(select) });
  }, []);

  const close = useCallback((refocus: boolean) => {
    setOpen(null);
    if (refocus) ref.current?.focus();
  }, []);

  function handleMouseDown(event: ReactMouseEvent<HTMLSelectElement>) {
    onMouseDown?.(event);
    if (event.defaultPrevented || event.button !== 0 || disabled) return;
    // Keeps the native list from opening; the themed one opens instead.
    event.preventDefault();
    if (open) close(true);
    else show();
  }

  function handleKeyDown(event: ReactKeyboardEvent<HTMLSelectElement>) {
    onKeyDown?.(event);
    if (event.defaultPrevented || disabled) return;
    const opens =
      event.key === " " ||
      event.key === "Enter" ||
      event.key === "F4" ||
      (event.altKey && (event.key === "ArrowDown" || event.key === "ArrowUp"));
    if (opens) {
      event.preventDefault();
      show();
    }
  }

  return (
    <>
      <select
        {...rest}
        ref={ref}
        disabled={disabled}
        aria-haspopup="listbox"
        aria-expanded={open ? true : undefined}
        aria-controls={open ? listId : undefined}
        onMouseDown={handleMouseDown}
        onKeyDown={handleKeyDown}
      >
        {children}
      </select>
      {open && ref.current ? (
        <SelectList
          id={listId}
          items={open.items}
          placement={open.placement}
          value={ref.current.value}
          owner={ref.current}
          onPick={(value) => {
            const select = ref.current;
            close(true);
            if (select) pick(select, value);
          }}
          onClose={close}
        />
      ) : null}
    </>
  );
}

interface SelectListProps {
  id: string;
  items: Item[];
  placement: Placement;
  value: string;
  owner: HTMLSelectElement;
  onPick: (value: string) => void;
  onClose: (refocus: boolean) => void;
}

function SelectList({ id, items, placement, value, owner, onPick, onClose }: SelectListProps) {
  const ref = useRef<HTMLDivElement>(null);
  const closeRef = useRef(onClose);
  closeRef.current = onClose;
  const enabled = items.map((item, i) => (item.disabled ? -1 : i)).filter((i) => i >= 0);
  const initial = items.findIndex((item) => item.value === value && !item.disabled);
  const [active, setActive] = useState(initial >= 0 ? initial : (enabled[0] ?? -1));
  const typed = useRef({ text: "", at: 0 });

  useLayoutEffect(() => {
    ref.current?.focus();
    ref.current
      ?.querySelector<HTMLElement>('[aria-selected="true"]')
      ?.scrollIntoView?.({ block: "nearest" });
  }, []);

  useEffect(() => {
    ref.current
      ?.querySelector<HTMLElement>(`[data-index="${active}"]`)
      ?.scrollIntoView?.({ block: "nearest" });
  }, [active]);

  useEffect(() => {
    const onDown = (event: MouseEvent) => {
      const target = event.target as Node;
      if (ref.current?.contains(target) || owner.contains(target)) return;
      closeRef.current(false);
    };
    // The list is positioned once; scrolling or resizing anything but the list moves the
    // control away from it, so it closes like a native popup.
    const onMove = (event: Event) => {
      if (event.target instanceof Node && ref.current?.contains(event.target)) return;
      closeRef.current(false);
    };
    const onBlur = () => closeRef.current(false);
    document.addEventListener("mousedown", onDown, true);
    window.addEventListener("scroll", onMove, true);
    window.addEventListener("resize", onMove);
    window.addEventListener("blur", onBlur);
    return () => {
      document.removeEventListener("mousedown", onDown, true);
      window.removeEventListener("scroll", onMove, true);
      window.removeEventListener("resize", onMove);
      window.removeEventListener("blur", onBlur);
    };
  }, [owner]);

  function step(delta: number) {
    if (enabled.length === 0) return;
    const at = enabled.indexOf(active);
    const next = at < 0 ? 0 : Math.max(0, Math.min(enabled.length - 1, at + delta));
    setActive(enabled[next] ?? active);
  }

  function typeahead(key: string) {
    const now = Date.now();
    const t = typed.current;
    t.text = now - t.at > 700 ? key : t.text + key;
    t.at = now;
    const text = t.text.toLowerCase();
    const from = Math.max(0, active);
    const order = [...items.keys()].map(
      (i) => (from + (t.text.length === 1 ? 1 : 0) + i) % items.length,
    );
    const hit = order.find(
      (i) => !items[i]?.disabled && items[i]?.label.toLowerCase().startsWith(text),
    );
    if (hit !== undefined) setActive(hit);
  }

  function onKeyDown(event: ReactKeyboardEvent<HTMLDivElement>) {
    switch (event.key) {
      case "ArrowDown":
        step(1);
        break;
      case "ArrowUp":
        if (event.altKey) {
          onClose(true);
          break;
        }
        step(-1);
        break;
      case "PageDown":
        step(8);
        break;
      case "PageUp":
        step(-8);
        break;
      case "Home":
        setActive(enabled[0] ?? -1);
        break;
      case "End":
        setActive(enabled[enabled.length - 1] ?? -1);
        break;
      case "Enter":
      case " ": {
        const item = items[active];
        if (item && !item.disabled) onPick(item.value);
        break;
      }
      case "Escape":
        onClose(true);
        break;
      case "Tab":
        onClose(true);
        return;
      default:
        if (event.key.length === 1 && !event.ctrlKey && !event.metaKey && !event.altKey) {
          typeahead(event.key);
          break;
        }
        return;
    }
    event.preventDefault();
    event.stopPropagation();
  }

  const up = placement.maxHeight < 0;
  const style = {
    left: placement.left,
    minWidth: placement.minWidth,
    maxHeight: Math.abs(placement.maxHeight),
    ...(up ? { bottom: window.innerHeight - placement.top } : { top: placement.top }),
  };

  let lastGroup: string | null = null;
  return createPortal(
    <div
      ref={ref}
      id={id}
      className="select-list"
      role="listbox"
      tabIndex={-1}
      aria-activedescendant={active >= 0 ? `${id}-${active}` : undefined}
      style={style}
      onKeyDown={onKeyDown}
    >
      {items.map((item, i) => {
        const heading =
          item.group !== null && item.group !== lastGroup ? (
            <div className="select-list__group" role="presentation">
              {item.group}
            </div>
          ) : null;
        lastGroup = item.group;
        const selected = item.value === value;
        return (
          // biome-ignore lint/suspicious/noArrayIndexKey: a fixed snapshot of the select's options; values may repeat
          <div key={`${i}:${item.value}`} role="presentation">
            {heading}
            {/* biome-ignore lint/a11y/useFocusableInteractive lint/a11y/useKeyWithClickEvents: listbox pattern: the list has focus and handles the keys (aria-activedescendant) */}
            <div
              id={`${id}-${i}`}
              data-index={i}
              role="option"
              aria-selected={selected}
              aria-disabled={item.disabled ? true : undefined}
              className={`select-list__option${i === active ? " select-list__option--active" : ""}${
                item.group !== null ? " select-list__option--grouped" : ""
              }`}
              onMouseMove={() => {
                if (!item.disabled && i !== active) setActive(i);
              }}
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => {
                if (!item.disabled) onPick(item.value);
              }}
            >
              <span className="select-list__check" aria-hidden="true">
                {selected ? <Check {...ICON_SM} /> : null}
              </span>
              <span className="select-list__label">{item.label || " "}</span>
            </div>
          </div>
        );
      })}
    </div>,
    document.body,
  );
}
