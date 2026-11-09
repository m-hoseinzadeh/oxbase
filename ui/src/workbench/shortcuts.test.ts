import { afterEach, describe, expect, it, vi } from "vitest";
import { onOpenSettings } from "../settings";
import { setDockviewApi } from "./layout";
import {
  appShortcutOf,
  installShortcuts,
  registerShortcuts,
  shortcutOf,
  zoomShortcutOf,
} from "./shortcuts";
import { getZoom, resetZoom } from "./zoom";

const keys = (
  key: string,
  mods: Partial<{ ctrl: boolean; meta: boolean; shift: boolean; alt: boolean }> = {},
) => ({
  key,
  ctrlKey: mods.ctrl ?? false,
  metaKey: mods.meta ?? false,
  shiftKey: mods.shift ?? false,
  altKey: mods.alt ?? false,
});

function press(key: string, init: KeyboardEventInit = {}): KeyboardEvent {
  const event = new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true, ...init });
  document.body.dispatchEvent(event);
  return event;
}

describe("shortcutOf", () => {
  it("maps the run, stop and reload keys", () => {
    expect(shortcutOf(keys("F5"))).toBe("run");
    expect(shortcutOf(keys("r", { ctrl: true }))).toBe("run");
    expect(shortcutOf(keys("R", { meta: true }))).toBe("run");
    expect(shortcutOf(keys("Enter", { ctrl: true }))).toBe("runCurrent");
    expect(shortcutOf(keys("Enter", { ctrl: true, shift: true }))).toBe("runAll");
    expect(shortcutOf(keys("F5", { shift: true }))).toBe("cancel");
    expect(shortcutOf(keys("F5", { ctrl: true }))).toBe("reload");
    expect(shortcutOf(keys("R", { ctrl: true, shift: true }))).toBe("reload");
    expect(shortcutOf(keys("s", { ctrl: true }))).toBe("save");
    expect(shortcutOf(keys("S", { ctrl: true, shift: true }))).toBe("saveAs");
    expect(shortcutOf(keys("Enter"))).toBeNull();
    expect(shortcutOf(keys("r"))).toBeNull();
    expect(shortcutOf(keys("F5", { alt: true }))).toBeNull();
  });
});

describe("installShortcuts", () => {
  let remove: (() => void) | undefined;
  let unregister: (() => void) | undefined;
  afterEach(() => {
    remove?.();
    unregister?.();
  });

  it("sends a shortcut to the active tab's handler", () => {
    const handler = vi.fn(() => true);
    unregister = registerShortcuts("tab-1", handler);
    remove = installShortcuts(() => "tab-1");
    expect(press("F5").defaultPrevented).toBe(true);
    expect(handler).toHaveBeenLastCalledWith("run");
    press("Enter", { ctrlKey: true, shiftKey: true });
    expect(handler).toHaveBeenLastCalledWith("runAll");
    press("F5", { shiftKey: true });
    expect(handler).toHaveBeenLastCalledWith("cancel");
  });

  it("never lets a reload key reload the app, even with no handler", () => {
    remove = installShortcuts(() => "nothing-here");
    expect(press("F5").defaultPrevented).toBe(true);
    expect(press("r", { ctrlKey: true }).defaultPrevented).toBe(true);
    expect(press("R", { ctrlKey: true, shiftKey: true }).defaultPrevented).toBe(true);
    expect(press("F5", { ctrlKey: true }).defaultPrevented).toBe(true);
    // Not a reload key and no handler: left to the browser.
    expect(press("Enter", { ctrlKey: true }).defaultPrevented).toBe(false);
  });

  it("leaves keys a focused component already handled, and other tabs' handlers", () => {
    const handler = vi.fn(() => true);
    unregister = registerShortcuts("tab-1", handler);
    remove = installShortcuts(() => "tab-2");
    press("F5");
    expect(handler).not.toHaveBeenCalled();

    remove();
    remove = installShortcuts(() => "tab-1");
    const tree = document.createElement("div");
    document.body.append(tree);
    tree.addEventListener("keydown", (event) => event.preventDefault());
    tree.dispatchEvent(
      new KeyboardEvent("keydown", { key: "F5", bubbles: true, cancelable: true }),
    );
    expect(handler).not.toHaveBeenCalled();
    tree.remove();
  });

  it("forgets a tab's handler when it unregisters", () => {
    const handler = vi.fn(() => true);
    registerShortcuts("tab-1", handler)();
    remove = installShortcuts(() => "tab-1");
    press("F5");
    expect(handler).not.toHaveBeenCalled();
  });
});

describe("zoom shortcuts", () => {
  afterEach(() => resetZoom());

  it("maps Ctrl with =, +, -, 0 on the main row and the keypad", () => {
    expect(zoomShortcutOf(keys("=", { ctrl: true }))).toBe("zoomIn");
    expect(zoomShortcutOf(keys("+", { ctrl: true, shift: true }))).toBe("zoomIn");
    expect(zoomShortcutOf({ ...keys("+", { ctrl: true }), code: "NumpadAdd" })).toBe("zoomIn");
    expect(zoomShortcutOf(keys("-", { meta: true }))).toBe("zoomOut");
    expect(zoomShortcutOf(keys("0", { ctrl: true }))).toBe("zoomReset");
    // Another layout's character on the same physical key still zooms.
    expect(zoomShortcutOf({ ...keys("ß", { ctrl: true }), code: "Minus" })).toBe("zoomOut");
    expect(zoomShortcutOf(keys("="))).toBeNull();
    expect(zoomShortcutOf(keys("0", { ctrl: true, alt: true }))).toBeNull();
  });

  it("zooms the app whichever tab is active", () => {
    const remove = installShortcuts(() => null);
    const event = press("=", { ctrlKey: true });
    expect(event.defaultPrevented).toBe(true);
    expect(getZoom()).toBe(1.1);
    press("-", { ctrlKey: true, repeat: true });
    press("-", { ctrlKey: true, repeat: true });
    expect(getZoom()).toBe(0.9);
    press("0", { ctrlKey: true });
    expect(getZoom()).toBe(1);
    remove();
  });
});

describe("app shortcuts", () => {
  it("maps Ctrl+N, Ctrl+O and Ctrl+,", () => {
    expect(shortcutOf(keys("n", { ctrl: true }))).toBe("new");
    expect(appShortcutOf(keys("o", { ctrl: true }))).toBe("openFile");
    expect(appShortcutOf(keys(",", { meta: true }))).toBe("settings");
    expect(appShortcutOf(keys("o", { ctrl: true, shift: true }))).toBeNull();
  });

  it("Ctrl+N opens a query tab unless the active tab uses it; Ctrl+, opens Settings", () => {
    const addPanel = vi.fn();
    setDockviewApi({ panels: [], addPanel } as never);
    const settings = vi.fn();
    const stop = onOpenSettings(settings);
    let used = false;
    const unregister = registerShortcuts("data-1", (command) => command === "new" && used);
    const remove = installShortcuts(() => "data-1");

    expect(press("n", { ctrlKey: true }).defaultPrevented).toBe(true);
    expect(addPanel).toHaveBeenCalledTimes(1);
    expect(addPanel.mock.calls[0]?.[0]).toMatchObject({ component: "query" });
    used = true;
    press("n", { ctrlKey: true });
    expect(addPanel).toHaveBeenCalledTimes(1);

    expect(press(",", { ctrlKey: true }).defaultPrevented).toBe(true);
    expect(settings).toHaveBeenCalledTimes(1);

    remove();
    unregister();
    stop();
    setDockviewApi(null);
  });
});
