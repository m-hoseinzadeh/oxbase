/**
 * The webview's own context menu (Back, Reload, Inspect, ...) offers nothing for the app, and
 * its Reload drops every open session. It stays only where it helps: editable text (cut, copy,
 * paste) and selected text (copy). Panels with a menu of their own (the object tree) and Monaco
 * call `preventDefault` themselves, so they are unaffected.
 */
const EDITABLE = 'input, textarea, select, [contenteditable=""], [contenteditable="true"]';

/** Whether the native context menu should open for `target` with `selection`. */
export function keepsNativeMenu(target: EventTarget | null, selection: string): boolean {
  if (selection.trim() !== "") return true;
  return target instanceof Element && target.closest(EDITABLE) !== null;
}

/** Installs the guard on `doc` (once per document); returns a function that removes it. */
export function installNativeMenuGuard(doc: Document = document): () => void {
  const onContextMenu = (event: MouseEvent) => {
    if (event.defaultPrevented) return;
    const selection = doc.getSelection()?.toString() ?? "";
    if (!keepsNativeMenu(event.target, selection)) event.preventDefault();
  };
  doc.addEventListener("contextmenu", onContextMenu);
  return () => doc.removeEventListener("contextmenu", onContextMenu);
}
