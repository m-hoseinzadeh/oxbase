/**
 * The tree view and the JSON view of one document (MongoDB), read with `document_at` as
 * canonical Extended JSON text. Field order is the document's own (see ./ejson).
 */
import { ChevronRight, CircleAlert, Copy, LoaderCircle } from "lucide-react";
import { type KeyboardEvent, useMemo, useState } from "react";
import { ICON_SM } from "../../components/icons";
import { type JsonNode, parseOrdered, prettyJson, viewOf } from "./ejson";

/** What the views show: the document's text, or why there is none. */
export type DocumentState =
  | { status: "none" }
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ok"; row: number; text: string };

function Placeholder({ state }: { state: DocumentState }) {
  if (state.status === "loading") {
    return (
      <div className="document-view__placeholder" role="status">
        <LoaderCircle {...ICON_SM} className="spin" />
        Loading the document…
      </div>
    );
  }
  if (state.status === "error") {
    return (
      <div className="document-view__placeholder document-view__placeholder--error" role="alert">
        <CircleAlert {...ICON_SM} />
        Could not read the document: {state.message}
      </div>
    );
  }
  return <p className="document-view__placeholder">Select a document in the grid.</p>;
}

interface TreeRow {
  path: string;
  level: number;
  key: string;
  type: string;
  text: string;
  expandable: boolean;
  expanded: boolean;
}

function flatten(node: JsonNode, expanded: ReadonlySet<string>): TreeRow[] {
  const rows: TreeRow[] = [];
  const walk = (entries: { key: string; value: JsonNode }[], parent: string, level: number) => {
    for (const entry of entries) {
      const path = parent ? `${parent}.${entry.key}` : entry.key;
      const view = viewOf(entry.value);
      const expandable = (view.children?.length ?? 0) > 0;
      const open = expandable && expanded.has(path);
      rows.push({
        path,
        level,
        key: entry.key,
        type: view.type,
        text: view.text,
        expandable,
        expanded: open,
      });
      if (open && view.children) walk(view.children, path, level + 1);
    }
  };
  const root = viewOf(node);
  walk(root.children ?? [{ key: "(value)", value: node }], "", 1);
  return rows;
}

/** The document as an expandable tree: field, value and BSON type per row. */
export function DocumentTree({ state }: { state: DocumentState }) {
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set());
  const [selected, setSelected] = useState<string | null>(null);
  const text = state.status === "ok" ? state.text : null;
  const parsed = useMemo(() => {
    if (text === null) return null;
    try {
      return { node: parseOrdered(text), error: null };
    } catch (error) {
      return { node: null, error: (error as Error).message };
    }
  }, [text]);
  if (state.status !== "ok") return <Placeholder state={state} />;
  if (!parsed?.node) {
    return <Placeholder state={{ status: "error", message: parsed?.error ?? "not JSON" }} />;
  }
  const rows = flatten(parsed.node, expanded);
  const toggle = (path: string) =>
    setExpanded((current) => {
      const next = new Set(current);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    const index = rows.findIndex((r) => r.path === selected);
    const row = rows[index];
    let target: TreeRow | undefined;
    switch (event.key) {
      case "ArrowDown":
        target = rows[Math.min(index + 1, rows.length - 1)];
        break;
      case "ArrowUp":
        target = rows[Math.max(index - 1, 0)];
        break;
      case "ArrowRight":
        if (row?.expandable && !row.expanded) toggle(row.path);
        break;
      case "ArrowLeft":
        if (row?.expanded) toggle(row.path);
        break;
      case "Enter":
      case " ":
        if (row?.expandable) toggle(row.path);
        break;
      default:
        return;
    }
    event.preventDefault();
    if (target) setSelected(target.path);
  }

  return (
    <div className="document-tree" data-testid="document-tree">
      <div className="document-tree__head" aria-hidden="true">
        <span>Field</span>
        <span>Value</span>
        <span>Type</span>
      </div>
      <div
        className="document-tree__rows"
        role="tree"
        aria-label="Document fields"
        tabIndex={0}
        aria-activedescendant={selected ? `doc-field-${selected}` : undefined}
        onKeyDown={onKeyDown}
      >
        {rows.map((row) => (
          // biome-ignore lint/a11y/useKeyWithClickEvents: the tree handles the keyboard.
          <div
            key={row.path}
            id={`doc-field-${row.path}`}
            role="treeitem"
            tabIndex={-1}
            aria-level={row.level}
            aria-selected={row.path === selected}
            {...(row.expandable ? { "aria-expanded": row.expanded } : {})}
            className="document-tree__row"
            onClick={() => {
              setSelected(row.path);
              if (row.expandable) toggle(row.path);
            }}
          >
            <span
              className="document-tree__key"
              style={{ paddingLeft: `${(row.level - 1) * 16 + 4}px` }}
            >
              <span
                className={`tree__twisty${row.expandable ? "" : " tree__twisty--leaf"}${row.expanded ? " tree__twisty--open" : ""}`}
                aria-hidden="true"
              >
                {row.expandable ? <ChevronRight {...ICON_SM} /> : null}
              </span>
              {row.key}
            </span>
            <span className={`document-tree__value document-tree__value--${row.type}`}>
              {row.text}
            </span>
            <span className="document-tree__type">{row.type}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

/** The document as indented Extended JSON (field order kept). */
export function DocumentJson({ state }: { state: DocumentState }) {
  const [copied, setCopied] = useState(false);
  if (state.status !== "ok") return <Placeholder state={state} />;
  const pretty = prettyJson(state.text);
  return (
    <section className="json-view document-json" aria-label="JSON view" data-testid="document-json">
      <div className="json-view__head">
        <span className="json-view__title">Document {state.row + 1} (Extended JSON)</span>
        <button
          type="button"
          className="button button--ghost button--small"
          onClick={() => {
            void navigator.clipboard?.writeText(pretty).then(
              () => setCopied(true),
              () => setCopied(false),
            );
          }}
        >
          <Copy {...ICON_SM} />
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      <pre className="json-view__text">{pretty}</pre>
    </section>
  );
}
