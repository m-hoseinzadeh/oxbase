import { afterEach, describe, expect, it } from "vitest";
import { installNativeMenuGuard, keepsNativeMenu } from "./native-menu";

function contextMenu(target: Element): MouseEvent {
  const event = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });
  target.dispatchEvent(event);
  return event;
}

describe("native context menu guard", () => {
  let remove: (() => void) | undefined;
  afterEach(() => {
    remove?.();
    document.body.innerHTML = "";
    document.getSelection()?.removeAllRanges();
  });

  it("keeps the menu on editable text and selected text only", () => {
    const input = document.createElement("input");
    const div = document.createElement("div");
    const editable = document.createElement("div");
    editable.setAttribute("contenteditable", "true");
    const inner = document.createElement("span");
    editable.append(inner);
    expect(keepsNativeMenu(input, "")).toBe(true);
    expect(keepsNativeMenu(document.createElement("textarea"), "")).toBe(true);
    expect(keepsNativeMenu(inner, "")).toBe(true);
    expect(keepsNativeMenu(div, "")).toBe(false);
    expect(keepsNativeMenu(div, "selected")).toBe(true);
    expect(keepsNativeMenu(div, "  ")).toBe(false);
    expect(keepsNativeMenu(null, "")).toBe(false);
  });

  it("cancels the webview menu elsewhere and leaves custom menus alone", () => {
    remove = installNativeMenuGuard();
    const panel = document.createElement("div");
    const field = document.createElement("input");
    document.body.append(panel, field);
    expect(contextMenu(panel).defaultPrevented).toBe(true);
    expect(contextMenu(field).defaultPrevented).toBe(false);

    // A panel with its own menu has already cancelled the event.
    const tree = document.createElement("div");
    let handled = false;
    tree.addEventListener("contextmenu", (event) => {
      event.preventDefault();
      handled = true;
    });
    document.body.append(tree);
    expect(contextMenu(tree).defaultPrevented).toBe(true);
    expect(handled).toBe(true);
  });

  it("is removed again", () => {
    installNativeMenuGuard()();
    const panel = document.createElement("div");
    document.body.append(panel);
    expect(contextMenu(panel).defaultPrevented).toBe(false);
  });
});
