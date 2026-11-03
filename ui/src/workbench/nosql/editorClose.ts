/**
 * Closing an editor session (`query_close_editor`) is asynchronous. A tab part that reuses an
 * editor id right after another part closed it (the key browser's value editor, one per key)
 * must wait for that close to land, or the close could end the new session.
 */
import { queryCloseEditor } from "../../api";

const closing = new Map<string, Promise<void>>();

/** Closes `editorId`'s session; later {@link editorReady} calls wait for it. */
export function closeEditorLater(editorId: string): void {
  const done: Promise<void> = queryCloseEditor(editorId)
    .catch(() => {})
    .finally(() => {
      if (closing.get(editorId) === done) closing.delete(editorId);
    });
  closing.set(editorId, done);
}

/** Resolves once no close of `editorId` is in flight. */
export async function editorReady(editorId: string): Promise<void> {
  for (let pending = closing.get(editorId); pending; pending = closing.get(editorId)) {
    await pending;
  }
}
