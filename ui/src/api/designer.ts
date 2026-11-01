/**
 * Typed wrappers for the M3 table designer commands of `crates/app-tauri` (src/designer.rs).
 * The UI edits a `TableDesign` (from `designerLoad`, or `newTableDesign` for a new table) and
 * sends it whole to `designerPreview` / `designerApply`.
 */
import { call } from "./call";
import type {
  DesignApplyOptions,
  DesignApplyReport,
  DesignPreview,
  EngineCapabilities,
  ObjectRef,
  TableDesign,
} from "./types";

/** The design of an existing table. */
export function designerLoad(
  sessionId: string,
  table: ObjectRef,
  requestId?: string,
): Promise<TableDesign> {
  return call("designer_load", { sessionId, table, requestId: requestId ?? null });
}

/** Diff, DDL script and issues of an edited design. Nothing runs. */
export function designerPreview(
  sessionId: string,
  design: TableDesign,
  requestId?: string,
): Promise<DesignPreview> {
  return call("designer_preview", { sessionId, design, requestId: requestId ?? null });
}

/** Applies an edited design; a destructive script needs `confirmDestructive`. */
export function designerApply(
  sessionId: string,
  design: TableDesign,
  options?: DesignApplyOptions,
  requestId?: string,
): Promise<DesignApplyReport> {
  return call("designer_apply", {
    sessionId,
    design,
    options: options ?? null,
    requestId: requestId ?? null,
  });
}

/** What the session's engine offers (designer operations, grid editing, NULLS ordering). */
export function capabilitiesGet(sessionId: string): Promise<EngineCapabilities> {
  return call("capabilities_get", { sessionId });
}
