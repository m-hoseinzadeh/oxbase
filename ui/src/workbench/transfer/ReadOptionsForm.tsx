/**
 * Format options of the import wizard (CSV, JSON, XLSX; Parquet has none). Every change is sent
 * to `import_preview` again by the wizard, so the preview follows what is chosen here.
 */
import { useId } from "react";
import type { ReadOptions, SourcePreview, TextEncoding, ValueFormats } from "../../api";

type CsvOptions = Extract<ReadOptions, { format: "csv" }>;
type JsonOptions = Extract<ReadOptions, { format: "json" }>;
type XlsxOptions = Extract<ReadOptions, { format: "xlsx" }>;

const DELIMITERS: { value: string; label: string }[] = [
  { value: ",", label: "Comma ( , )" },
  { value: ";", label: "Semicolon ( ; )" },
  { value: "\t", label: "Tab" },
  { value: "|", label: "Pipe ( | )" },
];

const ENCODINGS: { value: TextEncoding; label: string }[] = [
  { value: "auto", label: "Detect" },
  { value: "utf8", label: "UTF-8" },
  { value: "utf16le", label: "UTF-16 LE" },
  { value: "utf16be", label: "UTF-16 BE" },
  { value: "windows1252", label: "Windows-1252" },
  { value: "latin1", label: "ISO-8859-1 (Latin-1)" },
];

function CsvForm({
  options,
  onChange,
  disabled,
}: {
  options: CsvOptions;
  onChange: (next: CsvOptions) => void;
  disabled: boolean;
}) {
  const id = useId();
  const delimiter = options.delimiter ?? ",";
  const known = DELIMITERS.some((d) => d.value === delimiter);
  const nullTokens = options.nullTokens ?? [""];
  const emptyIsNull = nullTokens.includes("");
  const otherNull = nullTokens.find((t) => t !== "") ?? "";
  const decimalComma = options.formats?.decimalSeparator === ",";

  function setNulls(empty: boolean, other: string) {
    onChange({ ...options, nullTokens: [...(empty ? [""] : []), ...(other ? [other] : [])] });
  }

  return (
    <div className="transfer-form">
      <div className="field">
        <label htmlFor={`${id}-delimiter`}>Delimiter</label>
        <div className="transfer-form__inline">
          <select
            id={`${id}-delimiter`}
            value={known ? delimiter : "other"}
            disabled={disabled}
            onChange={(e) =>
              onChange({ ...options, delimiter: e.target.value === "other" ? "" : e.target.value })
            }
          >
            {DELIMITERS.map((d) => (
              <option key={d.label} value={d.value}>
                {d.label}
              </option>
            ))}
            <option value="other">Other…</option>
          </select>
          {known ? null : (
            <input
              type="text"
              aria-label="Other delimiter"
              className="transfer-form__char"
              maxLength={1}
              value={delimiter}
              disabled={disabled}
              onChange={(e) => onChange({ ...options, delimiter: e.target.value })}
            />
          )}
        </div>
      </div>
      <div className="field">
        <label htmlFor={`${id}-quote`}>Quote</label>
        <select
          id={`${id}-quote`}
          value={options.quote === null ? "none" : (options.quote ?? '"')}
          disabled={disabled}
          onChange={(e) =>
            onChange({ ...options, quote: e.target.value === "none" ? null : e.target.value })
          }
        >
          <option value={'"'}>Double quote ( " )</option>
          <option value="'">Single quote ( ' )</option>
          <option value="none">None</option>
        </select>
      </div>
      <div className="field">
        <label htmlFor={`${id}-encoding`}>Encoding</label>
        <select
          id={`${id}-encoding`}
          value={options.encoding ?? "auto"}
          disabled={disabled}
          onChange={(e) => onChange({ ...options, encoding: e.target.value as TextEncoding })}
        >
          {ENCODINGS.map((e) => (
            <option key={e.value} value={e.value}>
              {e.label}
            </option>
          ))}
        </select>
      </div>
      <div className="field">
        <label htmlFor={`${id}-skip`}>Skip lines at the start</label>
        <input
          id={`${id}-skip`}
          type="number"
          min={0}
          value={options.skipRows ?? 0}
          disabled={disabled}
          onChange={(e) =>
            onChange({ ...options, skipRows: Math.max(0, Math.floor(Number(e.target.value) || 0)) })
          }
        />
      </div>
      <div className="field">
        <label htmlFor={`${id}-decimal`}>Decimal separator</label>
        <select
          id={`${id}-decimal`}
          value={decimalComma ? "," : "."}
          disabled={disabled}
          onChange={(e) =>
            onChange({
              ...options,
              // Only the separator is sent: the core keeps its defaults for the rest
              // (`ValueFormats` fields all have serde defaults).
              formats: { ...options.formats, decimalSeparator: e.target.value } as ValueFormats,
            })
          }
        >
          <option value=".">Point ( 1.5 )</option>
          <option value=",">Comma ( 1,5 )</option>
        </select>
      </div>
      <div className="field">
        <label htmlFor={`${id}-null`}>Other text read as NULL</label>
        <input
          id={`${id}-null`}
          type="text"
          placeholder="for example \N or NULL"
          value={otherNull}
          disabled={disabled}
          onChange={(e) => setNulls(emptyIsNull, e.target.value)}
        />
      </div>
      <div className="transfer-form__checks">
        <label className="checkbox">
          <input
            type="checkbox"
            checked={options.header ?? true}
            disabled={disabled}
            onChange={(e) => onChange({ ...options, header: e.target.checked })}
          />
          First line holds the column names
        </label>
        <label className="checkbox">
          <input
            type="checkbox"
            checked={emptyIsNull}
            disabled={disabled}
            onChange={(e) => setNulls(e.target.checked, otherNull)}
          />
          Empty fields are NULL
        </label>
        <label className="checkbox">
          <input
            type="checkbox"
            checked={options.trim ?? false}
            disabled={disabled}
            onChange={(e) => onChange({ ...options, trim: e.target.checked })}
          />
          Trim spaces around values
        </label>
      </div>
    </div>
  );
}

function JsonForm({
  options,
  onChange,
  disabled,
}: {
  options: JsonOptions;
  onChange: (next: JsonOptions) => void;
  disabled: boolean;
}) {
  const id = useId();
  return (
    <div className="transfer-form">
      <div className="field">
        <label htmlFor={`${id}-layout`}>Layout</label>
        <select
          id={`${id}-layout`}
          value={options.layout ?? "auto"}
          disabled={disabled}
          onChange={(e) =>
            onChange({ ...options, layout: e.target.value as "auto" | "array" | "lines" })
          }
        >
          <option value="auto">Detect</option>
          <option value="array">One array of objects</option>
          <option value="lines">One object per line (NDJSON)</option>
        </select>
      </div>
      <div className="field">
        <label htmlFor={`${id}-encoding`}>Encoding</label>
        <select
          id={`${id}-encoding`}
          value={options.encoding ?? "auto"}
          disabled={disabled}
          onChange={(e) => onChange({ ...options, encoding: e.target.value as TextEncoding })}
        >
          {ENCODINGS.filter((e) => e.value === "auto" || e.value.startsWith("utf")).map((e) => (
            <option key={e.value} value={e.value}>
              {e.label}
            </option>
          ))}
        </select>
      </div>
    </div>
  );
}

function XlsxForm({
  options,
  sheets,
  onChange,
  disabled,
}: {
  options: XlsxOptions;
  sheets: string[];
  onChange: (next: XlsxOptions) => void;
  disabled: boolean;
}) {
  const id = useId();
  const sheet = options.sheet ?? "";
  return (
    <div className="transfer-form">
      <div className="field">
        <label htmlFor={`${id}-sheet`}>Sheet</label>
        <select
          id={`${id}-sheet`}
          value={sheet}
          disabled={disabled || sheets.length === 0}
          onChange={(e) => {
            const { sheet: _old, ...rest } = options;
            onChange(e.target.value ? { ...rest, sheet: e.target.value } : rest);
          }}
        >
          <option value="">First sheet</option>
          {sheets.map((s) => (
            <option key={s} value={s}>
              {s}
            </option>
          ))}
        </select>
      </div>
      <div className="field">
        <label htmlFor={`${id}-header`}>Header row</label>
        <input
          id={`${id}-header`}
          type="number"
          min={0}
          value={options.headerRow ?? 1}
          disabled={disabled}
          onChange={(e) =>
            onChange({
              ...options,
              headerRow: Math.max(0, Math.floor(Number(e.target.value) || 0)),
            })
          }
        />
        <p className="field__hint">0 = the sheet has no header row.</p>
      </div>
    </div>
  );
}

export function ReadOptionsForm({
  options,
  preview,
  onChange,
  disabled = false,
}: {
  options: ReadOptions;
  preview: SourcePreview | null;
  onChange: (next: ReadOptions) => void;
  disabled?: boolean;
}) {
  switch (options.format) {
    case "csv":
      return <CsvForm options={options} onChange={onChange} disabled={disabled} />;
    case "json":
      return <JsonForm options={options} onChange={onChange} disabled={disabled} />;
    case "xlsx":
      return (
        <XlsxForm
          options={options}
          sheets={preview?.detected.sheets ?? []}
          onChange={onChange}
          disabled={disabled}
        />
      );
    case "parquet":
      return <p className="field__hint">Parquet files carry their own column names and types.</p>;
  }
}
