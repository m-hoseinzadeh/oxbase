/**
 * 1.5: which table-data tabs hold pending changes that were not applied yet, so quitting can
 * say what would be lost (workbench/documents/appEvents.ts). Each tab reports its count while it
 * is open (`TableDataPanel`); unsaved query tabs are not counted here, because the workspace
 * keeps their text.
 */

const pending = new Map<string, { table: string; count: number }>();

/** Tab `editorId` (table `table`) now has `count` pending changes; 0 forgets it. */
export function noteUnappliedChanges(editorId: string, table: string, count: number): void {
  if (count > 0) pending.set(editorId, { table, count });
  else pending.delete(editorId);
}

/** The tables with pending changes, one entry per tab. */
export function tablesWithUnappliedChanges(): { table: string; count: number }[] {
  return [...pending.values()];
}

/** Tests only. */
export function resetUnappliedChanges(): void {
  pending.clear();
}
