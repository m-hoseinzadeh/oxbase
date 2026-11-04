/**
 * The value panel next to a result grid: the selected cell's whole value, read-only, in an
 * editor (long text scrolls, wraps, folds and can be searched), with a beautifier.
 *
 * Formats: Auto (JSON and XML by shape, else as it is), Text, JSON and XML (re-indented
 * without changing a value, ./valueFormat) and SQL (the core's formatter for the connection's
 * engine). A value the chosen beautifier cannot read is shown as it is, with a note.
 */
import { Check, Copy, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { type ColumnInfo, type Engine, formatSql } from "../../api";
import { ICON_SM } from "../../components/icons";
import { Select } from "../../components/Select";
import { hasInsight } from "../editor/insight/controller";
import { languageFor } from "../editor/language";
import { type EditorLanguage, SqlEditor, type SqlEditorHandle } from "../editor/SqlEditor";
import { columnTypeLabel } from "./format";
import { detectFormat, prettyJson, prettyXml, type ValueFormat } from "./valueFormat";

export type FormatChoice = "auto" | ValueFormat;

export interface ValuePanelProps {
  /** The selected cell's column; `null` = no cell selected. */
  column: ColumnInfo | null;
  /** The value: text, `null` (SQL NULL) or `undefined` (no cell, or its row is loading). */
  value: string | null | undefined;
  /** The connection's engine, for the SQL beautifier and grammar. */
  engine?: Engine | null | undefined;
  onClose: () => void;
}

interface Shown {
  text: string;
  language: EditorLanguage;
  note: string | null;
}

function languageOf(format: ValueFormat, engine: Engine | null | undefined): EditorLanguage {
  switch (format) {
    case "json":
      return "javascript";
    case "xml":
      return "xml";
    case "sql":
      return languageFor(engine ?? undefined);
    default:
      return "plaintext";
  }
}

/** The value in `format`, synchronously (SQL is formatted by the core, see below). */
function shownSync(
  value: string,
  choice: FormatChoice,
): { format: ValueFormat; shown: Shown | null } {
  const format = choice === "auto" ? detectFormat(value) : choice;
  if (format === "json") {
    const pretty = prettyJson(value);
    return {
      format,
      shown: {
        text: pretty ?? value,
        language: "javascript",
        note: pretty === null ? "Not valid JSON: shown as it is." : null,
      },
    };
  }
  if (format === "xml") {
    const pretty = prettyXml(value);
    return {
      format,
      shown: {
        text: pretty ?? value,
        language: "xml",
        note: pretty === null ? "Not well-formed XML: shown as it is." : null,
      },
    };
  }
  if (format === "sql") return { format, shown: null };
  return { format, shown: { text: value, language: "plaintext", note: null } };
}

const FORMATS: { value: FormatChoice; label: string }[] = [
  { value: "auto", label: "Auto" },
  { value: "text", label: "Text" },
  { value: "json", label: "JSON" },
  { value: "xml", label: "XML" },
  { value: "sql", label: "SQL" },
];

export function ValuePanel({ column, value, engine, onClose }: ValuePanelProps) {
  const [choice, setChoice] = useState<FormatChoice>("auto");
  const [wrap, setWrap] = useState(true);
  const [copied, setCopied] = useState(false);
  const [sql, setSql] = useState<{ source: string; shown: Shown } | null>(null);
  const editorRef = useRef<SqlEditorHandle | null>(null);

  const sync = typeof value === "string" ? shownSync(value, choice) : null;
  const wantsSql = typeof value === "string" && sync?.format === "sql";

  // SQL is formatted by the core; until it answers (or without a formatter) the value shows as is.
  useEffect(() => {
    if (!wantsSql || typeof value !== "string") return;
    if (!engine || !hasInsight(engine)) {
      setSql({
        source: value,
        shown: {
          text: value,
          language: languageOf("sql", engine),
          note: "No SQL formatter for this connection: shown as it is.",
        },
      });
      return;
    }
    let live = true;
    formatSql({ engine, text: value, range: null, options: null }).then(
      (answer) => {
        if (live) {
          setSql({
            source: value,
            shown: { text: answer.text, language: languageOf("sql", engine), note: null },
          });
        }
      },
      (error: unknown) => {
        if (!live) return;
        const message = (error as { message?: string })?.message ?? String(error);
        setSql({
          source: value,
          shown: {
            text: value,
            language: languageOf("sql", engine),
            note: `Could not format: ${message}`,
          },
        });
      },
    );
    return () => {
      live = false;
    };
  }, [wantsSql, value, engine]);

  const shown: Shown | null =
    typeof value !== "string"
      ? null
      : (sync?.shown ??
        (sql && sql.source === value
          ? sql.shown
          : { text: value, language: languageOf("sql", engine), note: "Formatting…" }));

  useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(false), 1500);
    return () => clearTimeout(timer);
  }, [copied]);

  async function copy() {
    if (!shown) return;
    try {
      await navigator.clipboard.writeText(shown.text);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  }

  const detected = choice === "auto" && sync ? sync.format : null;
  // The stored value's size, not the beautified text's.
  const lines = typeof value === "string" ? value.split("\n").length : 0;

  return (
    <aside
      className="value-panel"
      aria-label="Value"
      data-testid="value-panel"
      onKeyDown={(event) => {
        if (event.key === "Escape") {
          event.stopPropagation();
          onClose();
        }
      }}
    >
      <header className="value-panel__header">
        <div className="value-panel__title">
          <span className="value-panel__name">{column ? column.name : "Value"}</span>
          {column ? <span className="value-panel__type">{columnTypeLabel(column)}</span> : null}
        </div>
        <button
          type="button"
          className="icon-button"
          aria-label="Close the value panel"
          title="Close (Esc)"
          onClick={onClose}
        >
          <X {...ICON_SM} />
        </button>
      </header>
      <div className="value-panel__toolbar">
        <Select
          aria-label="Format"
          className="value-panel__format"
          value={choice}
          onChange={(e) => setChoice(e.target.value as FormatChoice)}
        >
          {FORMATS.map((f) => (
            <option key={f.value} value={f.value}>
              {f.value === "auto" && detected ? `Auto (${detected.toUpperCase()})` : f.label}
            </option>
          ))}
        </Select>
        <label className="checkbox value-panel__wrap">
          <input type="checkbox" checked={wrap} onChange={(e) => setWrap(e.target.checked)} />
          Wrap
        </label>
        <button
          type="button"
          className="button button--ghost button--small"
          disabled={!shown}
          onClick={() => void copy()}
        >
          {copied ? <Check {...ICON_SM} /> : <Copy {...ICON_SM} />}
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      {value === undefined ? (
        <p className="value-panel__empty">
          {column ? "Loading…" : "Select a cell to see its whole value."}
        </p>
      ) : value === null ? (
        <p className="value-panel__empty value-panel__null">NULL</p>
      ) : (
        <>
          {shown?.note ? (
            <p className="value-panel__note" role="status">
              {shown.note}
            </p>
          ) : null}
          <div className="value-panel__body">
            <SqlEditor
              initialValue=""
              value={shown?.text ?? ""}
              language={shown?.language ?? "plaintext"}
              editorRef={editorRef}
              readOnly
              wordWrap={wrap}
              ariaLabel={column ? `Value of ${column.name}` : "Value"}
              testId="value-panel-text"
            />
          </div>
          <p className="value-panel__stats">
            {value.length.toLocaleString()} characters · {lines.toLocaleString()}{" "}
            {lines === 1 ? "line" : "lines"}
          </p>
        </>
      )}
    </aside>
  );
}
