/**
 * A cell's whole value, read-only: the body of the value panel (./ValuePanel) and of the value
 * dialog (./ValueDialog). Long text scrolls, wraps, folds and can be searched in an editor.
 *
 * Formats: Auto (JSON, HTML and XML by shape, else as it is), Text, JSON and XML (re-indented
 * without changing a value, ./valueFormat), HTML (highlighted source, or a safe preview: no
 * scripts, no network) and SQL (the core's formatter for the connection's engine). A binary
 * value that is an image (PNG, JPEG, GIF, WebP, BMP) shows as the image. A value the chosen
 * beautifier cannot read is shown as it is, with a note.
 */
import { Check, Copy } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { type Engine, formatSql } from "../../api";
import { ICON_SM } from "../../components/icons";
import { Select } from "../../components/Select";
import { hasInsight } from "../editor/insight/controller";
import { languageFor } from "../editor/language";
import { type EditorLanguage, SqlEditor, type SqlEditorHandle } from "../editor/SqlEditor";
import { detectFormat, imageMime, prettyJson, prettyXml, type ValueFormat } from "./valueFormat";

export type FormatChoice = "auto" | ValueFormat | "image";

export interface ValueViewProps {
  /** The value: text, `null` (SQL NULL) or `undefined` (no cell, or its row is loading). */
  value: string | null | undefined;
  /** The raw bytes of a binary value (an image shows as the image). */
  bytes?: Uint8Array | null | undefined;
  /** The connection's engine, for the SQL beautifier and grammar. */
  engine?: Engine | null | undefined;
  /** Shown instead of the value while there is none (`value === undefined`). */
  empty: string;
  /** The editor's accessible name. */
  label: string;
  testId: string;
}

interface Shown {
  text: string;
  language: EditorLanguage;
  note: string | null;
}

/**
 * Wrapped around previewed HTML: nothing loads from the network and no script runs (the frame
 * is also sandboxed without scripts), only inline styles and `data:` images.
 */
const PREVIEW_CSP =
  "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src data:\">";

function languageOf(format: ValueFormat, engine: Engine | null | undefined): EditorLanguage {
  switch (format) {
    case "json":
      return "javascript";
    case "xml":
      return "xml";
    case "html":
      return "html";
    case "sql":
      return languageFor(engine ?? undefined);
    default:
      return "plaintext";
  }
}

/** The value in `format`, synchronously (SQL is formatted by the core, see below). */
function shownSync(value: string, format: ValueFormat): Shown | null {
  if (format === "json") {
    const pretty = prettyJson(value);
    return {
      text: pretty ?? value,
      language: "javascript",
      note: pretty === null ? "Not valid JSON: shown as it is." : null,
    };
  }
  if (format === "xml") {
    const pretty = prettyXml(value);
    return {
      text: pretty ?? value,
      language: "xml",
      note: pretty === null ? "Not well-formed XML: shown as it is." : null,
    };
  }
  // HTML is often not well-formed XML (void tags, unclosed <p>): then it shows as it is.
  if (format === "html") return { text: prettyXml(value) ?? value, language: "html", note: null };
  if (format === "sql") return null;
  return { text: value, language: "plaintext", note: null };
}

const FORMATS: { value: FormatChoice; label: string }[] = [
  { value: "auto", label: "Auto" },
  { value: "text", label: "Text" },
  { value: "json", label: "JSON" },
  { value: "xml", label: "XML" },
  { value: "html", label: "HTML" },
  { value: "sql", label: "SQL" },
];

/** An object URL for `bytes` as `mime`, revoked when it changes or the view goes away. */
function useObjectUrl(bytes: Uint8Array | null | undefined, mime: string | null): string | null {
  const url = useMemo(() => {
    if (!bytes || !mime || typeof URL.createObjectURL !== "function") return null;
    return URL.createObjectURL(new Blob([bytes as BlobPart], { type: mime }));
  }, [bytes, mime]);
  useEffect(() => () => (url ? URL.revokeObjectURL(url) : undefined), [url]);
  return url;
}

export function ValueView({ value, bytes, engine, empty, label, testId }: ValueViewProps) {
  const [choice, setChoice] = useState<FormatChoice>("auto");
  const [wrap, setWrap] = useState(true);
  const [preview, setPreview] = useState(false);
  const [copied, setCopied] = useState(false);
  const [sql, setSql] = useState<{ source: string; shown: Shown } | null>(null);
  const editorRef = useRef<SqlEditorHandle | null>(null);

  const mime = imageMime(bytes);
  const imageUrl = useObjectUrl(bytes, mime);
  const showImage = imageUrl !== null && (choice === "image" || choice === "auto");
  const format: ValueFormat | null =
    typeof value !== "string" || showImage
      ? null
      : choice === "auto" || choice === "image"
        ? detectFormat(value)
        : choice;
  const sync = typeof value === "string" && format ? shownSync(value, format) : null;
  const wantsSql = format === "sql";

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
    typeof value !== "string" || format === null
      ? null
      : (sync ??
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

  const detected = choice === "auto" ? (showImage ? "image" : format) : null;
  const choices = mime ? [...FORMATS, { value: "image" as const, label: "Image" }] : FORMATS;
  // The stored value's size, not the beautified text's.
  const lines = typeof value === "string" ? value.split("\n").length : 0;
  const previewing = format === "html" && preview && typeof value === "string";

  return (
    <>
      <div className="value-panel__toolbar">
        <Select
          aria-label="Format"
          className="value-panel__format"
          value={choice}
          onChange={(e) => setChoice(e.target.value as FormatChoice)}
        >
          {choices.map((f) => (
            <option key={f.value} value={f.value}>
              {f.value === "auto" && detected ? `Auto (${detected.toUpperCase()})` : f.label}
            </option>
          ))}
        </Select>
        <span className="value-panel__options">
          {format === "html" ? (
            <label className="checkbox">
              <input
                type="checkbox"
                checked={preview}
                onChange={(e) => setPreview(e.target.checked)}
              />
              Preview
            </label>
          ) : null}
          {shown && !previewing ? (
            <label className="checkbox">
              <input type="checkbox" checked={wrap} onChange={(e) => setWrap(e.target.checked)} />
              Wrap
            </label>
          ) : null}
        </span>
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
        <p className="value-panel__empty">{empty}</p>
      ) : value === null ? (
        <p className="value-panel__empty value-panel__null">NULL</p>
      ) : showImage && imageUrl ? (
        <div className="value-panel__body value-panel__image" data-testid={`${testId}-image`}>
          <img src={imageUrl} alt={`${label} (${mime})`} />
        </div>
      ) : (
        <>
          {shown?.note ? (
            <p className="value-panel__note" role="status">
              {shown.note}
            </p>
          ) : null}
          <div className="value-panel__body">
            {previewing ? (
              <iframe
                className="value-panel__preview"
                title={`${label}, HTML preview`}
                sandbox=""
                srcDoc={`${PREVIEW_CSP}${value}`}
                data-testid={`${testId}-preview`}
              />
            ) : (
              <SqlEditor
                initialValue=""
                value={shown?.text ?? ""}
                language={shown?.language ?? "plaintext"}
                editorRef={editorRef}
                readOnly
                wordWrap={wrap}
                ariaLabel={label}
                testId={testId}
              />
            )}
          </div>
        </>
      )}
      {typeof value === "string" ? (
        <p className="value-panel__stats">
          {mime && bytes ? `${mime} · ${bytes.length.toLocaleString()} bytes · ` : ""}
          {value.length.toLocaleString()} characters · {lines.toLocaleString()}{" "}
          {lines === 1 ? "line" : "lines"}
        </p>
      ) : null}
    </>
  );
}
