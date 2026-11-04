import { afterEach, describe, expect, it, vi } from "vitest";
import { installShortcuts, registerShortcuts, shortcutOf } from "./shortcuts";

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
