/** The schema cache's state in the query editor's toolbar, with a refresh button. */
import type { CacheStatus } from "../../../api";
import type { SchemaCacheView } from "./useSchemaCache";

const COUNT = new Intl.NumberFormat("en-US");

function plural(n: number, one: string, many: string): string {
  return `${COUNT.format(n)} ${n === 1 ? one : many}`;
}

/** The indicator's state name (`data-state`) and text. */
export function describeCache(
  view: Pick<SchemaCacheView, "status" | "error" | "refreshing">,
  connected: boolean,
): { state: string; label: string; detail: string } {
  const { status, error, refreshing } = view;
  if (!connected) return { state: "none", label: "Schema: no connection", detail: "" };
  if (refreshing || status?.building) {
    return {
      state: "building",
      label: "Schema: loading…",
      detail: "Reading the database's tables, columns and routines.",
    };
  }
  if (!status) {
    if (error) return { state: "error", label: "Schema: unavailable", detail: error.message };
    return { state: "unknown", label: "Schema: …", detail: "" };
  }
  const counts = `${plural(status.relationCount, "table", "tables")}`;
  const detail = details(status, error?.message ?? null);
  switch (status.state) {
    case "missing":
      return {
        state: "missing",
        label: "Schema: not loaded",
        detail: detail || "Refresh to read the schema.",
      };
    case "ready":
      return {
        state: "ready",
        label: `Schema: ${counts}${status.truncated ? " (partial)" : ""}`,
        detail,
      };
    case "stale":
      return { state: "stale", label: `Schema: ${counts} (stale)`, detail };
    case "failed":
      return { state: "failed", label: "Schema: failed", detail };
    default:
      return { state: "unknown", label: "Schema: …", detail: "" };
  }
}

function details(status: CacheStatus, error: string | null): string {
  const parts: string[] = [];
  if (status.state !== "missing") {
    parts.push(
      `${plural(status.relationCount, "table or view", "tables and views")}, ` +
        `${plural(status.columnCount, "column", "columns")}, ` +
        `${plural(status.routineCount, "routine", "routines")}`,
    );
  }
  if (status.truncated) parts.push("Only part of the schema was read (it is very large).");
  if (status.readAt) {
    const when = new Date(status.readAt);
    const text = Number.isNaN(when.getTime()) ? status.readAt : when.toLocaleString();
    parts.push(`Read ${text}${status.source === "disk" ? " (saved copy)" : ""}`);
  }
  if (status.lastBuildMs !== null)
    parts.push(`Last read took ${COUNT.format(status.lastBuildMs)} ms`);
  const failure =
    status.lastError?.message ?? (status.lastError ? status.lastError.category : null);
  if (failure) parts.push(`Last error: ${failure}`);
  if (error) parts.push(error);
  return parts.join("\n");
}

export function SchemaCacheIndicator({
  view,
  connected,
}: {
  view: SchemaCacheView;
  connected: boolean;
}) {
  const { state, label, detail } = describeCache(view, connected);
  return (
    <span className="schema-cache">
      <span
        className="schema-cache__status"
        data-testid="schema-cache-status"
        data-state={state}
        title={detail || undefined}
        aria-live="polite"
      >
        <span className="schema-cache__dot" aria-hidden="true" />
        {label}
      </span>
      <button
        type="button"
        className="button button--small schema-cache__refresh"
        data-testid="schema-cache-refresh"
        aria-label="Refresh schema cache"
        title="Read the schema again (tables, columns, routines) for completion"
        disabled={!connected || view.refreshing}
        onClick={view.refresh}
      >
        ↻
      </button>
    </span>
  );
}
