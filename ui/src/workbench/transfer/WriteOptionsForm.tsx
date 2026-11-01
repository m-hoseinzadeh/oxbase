/** Format options of the export dialog (CSV, JSON, XLSX, Parquet, SQL dump). */
import { useId } from "react";
import type {
  Engine,
  LineEnding,
  ParquetCompression,
  QuoteStyle,
  TextEncoding,
  WriteOptions,
} from "../../api";
import { ENGINE_INFO } from "../../connections/types";
import { LEVEL_RANGE, SQL_DUMP_ENGINES } from "./model";

type Csv = Extract<WriteOptions, { format: "csv" }>;
type Json = Extract<WriteOptions, { format: "json" }>;
type Xlsx = Extract<WriteOptions, { format: "xlsx" }>;
type Parquet = Extract<WriteOptions, { format: "parquet" }>;
type Dump = Extract<WriteOptions, { format: "sql_dump" }>;

const ENCODINGS: { value: TextEncoding; label: string }[] = [
  { value: "utf8", label: "UTF-8" },
  { value: "utf16le", label: "UTF-16 LE" },
  { value: "utf16be", label: "UTF-16 BE" },
  { value: "windows1252", label: "Windows-1252" },
  { value: "latin1", label: "ISO-8859-1 (Latin-1)" },
];

const CODECS: ParquetCompression["codec"][] = [
  "snappy",
  "zstd",
  "gzip",
  "brotli",
  "lz4_raw",
  "none",
];

function Check({
  label,
  checked,
  onChange,
  disabled,
}: {
  label: string;
  checked: boolean;
  onChange: (value: boolean) => void;
  disabled: boolean;
}) {
  return (
    <label className="checkbox">
      <input
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={(e) => onChange(e.target.checked)}
      />
      {label}
    </label>
  );
}

function CsvForm({ o, set, disabled }: { o: Csv; set: (n: Csv) => void; disabled: boolean }) {
  const id = useId();
  return (
    <div className="transfer-form">
      <div className="field">
        <label htmlFor={`${id}-delimiter`}>Delimiter</label>
        <select
          id={`${id}-delimiter`}
          value={o.delimiter ?? ","}
          disabled={disabled}
          onChange={(e) => set({ ...o, delimiter: e.target.value })}
        >
          <option value=",">Comma ( , )</option>
          <option value=";">Semicolon ( ; )</option>
          <option value={"\t"}>Tab</option>
          <option value="|">Pipe ( | )</option>
        </select>
      </div>
      <div className="field">
        <label htmlFor={`${id}-quoting`}>Quote values</label>
        <select
          id={`${id}-quoting`}
          value={o.quoteStyle ?? "necessary"}
          disabled={disabled}
          onChange={(e) => set({ ...o, quoteStyle: e.target.value as QuoteStyle })}
        >
          <option value="necessary">When needed</option>
          <option value="always">Always</option>
          <option value="non_numeric">All but numbers</option>
          <option value="never">Never</option>
        </select>
      </div>
      <div className="field">
        <label htmlFor={`${id}-encoding`}>Encoding</label>
        <select
          id={`${id}-encoding`}
          value={o.encoding ?? "utf8"}
          disabled={disabled}
          onChange={(e) => set({ ...o, encoding: e.target.value as TextEncoding })}
        >
          {ENCODINGS.map((e) => (
            <option key={e.value} value={e.value}>
              {e.label}
            </option>
          ))}
        </select>
      </div>
      <div className="field">
        <label htmlFor={`${id}-eol`}>Line ends</label>
        <select
          id={`${id}-eol`}
          value={o.lineEnding ?? "lf"}
          disabled={disabled}
          onChange={(e) => set({ ...o, lineEnding: e.target.value as LineEnding })}
        >
          <option value="lf">LF (Linux, macOS)</option>
          <option value="cr_lf">CR LF (Windows)</option>
        </select>
      </div>
      <div className="field">
        <label htmlFor={`${id}-null`}>NULL is written as</label>
        <input
          id={`${id}-null`}
          type="text"
          value={o.nullToken ?? ""}
          placeholder="(empty)"
          disabled={disabled}
          onChange={(e) => set({ ...o, nullToken: e.target.value })}
        />
      </div>
      <div className="transfer-form__checks">
        <Check
          label="Column names in the first line"
          checked={o.header ?? true}
          disabled={disabled}
          onChange={(header) => set({ ...o, header })}
        />
        <Check
          label="Byte order mark (BOM)"
          checked={o.bom ?? false}
          disabled={disabled}
          onChange={(bom) => set({ ...o, bom })}
        />
        <Check
          label="Protect against spreadsheet formulas (prefix =, +, -, @ with ')"
          checked={o.formulaGuard ?? true}
          disabled={disabled}
          onChange={(formulaGuard) => set({ ...o, formulaGuard })}
        />
      </div>
    </div>
  );
}

function JsonForm({ o, set, disabled }: { o: Json; set: (n: Json) => void; disabled: boolean }) {
  const id = useId();
  return (
    <div className="transfer-form">
      <div className="field">
        <label htmlFor={`${id}-layout`}>Layout</label>
        <select
          id={`${id}-layout`}
          value={o.layout ?? "array"}
          disabled={disabled}
          onChange={(e) => set({ ...o, layout: e.target.value as "array" | "lines" })}
        >
          <option value="array">One array of objects</option>
          <option value="lines">One object per line (NDJSON)</option>
        </select>
      </div>
      <div className="field">
        <label htmlFor={`${id}-binary`}>Binary values</label>
        <select
          id={`${id}-binary`}
          value={o.binary ?? "base64"}
          disabled={disabled}
          onChange={(e) => set({ ...o, binary: e.target.value as "base64" | "hex" })}
        >
          <option value="base64">Base64</option>
          <option value="hex">Hex</option>
        </select>
      </div>
      <div className="transfer-form__checks">
        <Check
          label="Indented (pretty)"
          checked={o.pretty ?? false}
          disabled={disabled}
          onChange={(pretty) => set({ ...o, pretty })}
        />
        <Check
          label="Write NULL fields"
          checked={o.explicitNulls ?? true}
          disabled={disabled}
          onChange={(explicitNulls) => set({ ...o, explicitNulls })}
        />
      </div>
    </div>
  );
}

function XlsxForm({ o, set, disabled }: { o: Xlsx; set: (n: Xlsx) => void; disabled: boolean }) {
  const id = useId();
  return (
    <div className="transfer-form">
      <div className="field">
        <label htmlFor={`${id}-sheet`}>Sheet name</label>
        <input
          id={`${id}-sheet`}
          type="text"
          value={o.sheetName ?? "Sheet1"}
          disabled={disabled}
          onChange={(e) => set({ ...o, sheetName: e.target.value })}
        />
        <p className="field__hint">More than 1,048,576 rows continue on further sheets.</p>
      </div>
      <div className="transfer-form__checks">
        <Check
          label="Column names in the first row"
          checked={o.header ?? true}
          disabled={disabled}
          onChange={(header) => set({ ...o, header })}
        />
        <Check
          label="Write text that looks like a formula as text"
          checked={o.formulaGuard ?? true}
          disabled={disabled}
          onChange={(formulaGuard) => set({ ...o, formulaGuard })}
        />
      </div>
    </div>
  );
}

function ParquetForm({
  o,
  set,
  disabled,
}: {
  o: Parquet;
  set: (n: Parquet) => void;
  disabled: boolean;
}) {
  const id = useId();
  const compression = o.compression ?? { codec: "snappy" };
  const range = LEVEL_RANGE[compression.codec];
  const level = "level" in compression ? compression.level : undefined;
  return (
    <div className="transfer-form">
      <div className="field">
        <label htmlFor={`${id}-codec`}>Compression</label>
        <select
          id={`${id}-codec`}
          value={compression.codec}
          disabled={disabled}
          onChange={(e) =>
            set({ ...o, compression: { codec: e.target.value } as ParquetCompression })
          }
        >
          {CODECS.map((c) => (
            <option key={c} value={c}>
              {c === "none" ? "None" : c === "lz4_raw" ? "LZ4" : c}
            </option>
          ))}
        </select>
      </div>
      {range ? (
        <div className="field">
          <label htmlFor={`${id}-level`}>Level</label>
          <input
            id={`${id}-level`}
            type="number"
            min={range[0]}
            max={range[1]}
            placeholder="default"
            value={level ?? ""}
            disabled={disabled}
            onChange={(e) => {
              const text = e.target.value;
              set({
                ...o,
                compression: (text === ""
                  ? { codec: compression.codec }
                  : { codec: compression.codec, level: Number(text) }) as ParquetCompression,
              });
            }}
          />
          <p className="field__hint">
            {range[0]} to {range[1]}; empty = the codec's default.
          </p>
        </div>
      ) : null}
    </div>
  );
}

function DumpForm({
  o,
  set,
  disabled,
  defaultTable,
}: {
  o: Dump;
  set: (n: Dump) => void;
  disabled: boolean;
  defaultTable: string;
}) {
  const id = useId();
  return (
    <div className="transfer-form">
      <div className="field">
        <label htmlFor={`${id}-target`}>SQL for</label>
        <select
          id={`${id}-target`}
          value={o.target}
          disabled={disabled}
          onChange={(e) => {
            const { targetVersion: _version, ...rest } = o;
            set({ ...rest, target: e.target.value as Engine });
          }}
        >
          {SQL_DUMP_ENGINES.map((e) => (
            <option key={e} value={e}>
              {ENGINE_INFO[e].label}
            </option>
          ))}
        </select>
      </div>
      <div className="field">
        <label htmlFor={`${id}-version`}>Server version</label>
        <input
          id={`${id}-version`}
          type="text"
          placeholder="latest"
          value={o.targetVersion ?? ""}
          disabled={disabled}
          onChange={(e) => {
            const { targetVersion: _version, ...rest } = o;
            set(e.target.value ? { ...rest, targetVersion: e.target.value } : rest);
          }}
        />
      </div>
      <div className="field">
        <label htmlFor={`${id}-table`}>Table name in the dump</label>
        <input
          id={`${id}-table`}
          type="text"
          value={o.table?.name ?? defaultTable}
          disabled={disabled}
          onChange={(e) => {
            const { table: _table, ...rest } = o;
            set(
              e.target.value === defaultTable
                ? rest
                : { ...rest, table: { kind: "table", name: e.target.value } },
            );
          }}
        />
      </div>
      <div className="field">
        <label htmlFor={`${id}-rows`}>Rows per INSERT</label>
        <input
          id={`${id}-rows`}
          type="number"
          min={1}
          max={10000}
          value={o.rowsPerInsert ?? 100}
          disabled={disabled}
          onChange={(e) => set({ ...o, rowsPerInsert: Math.floor(Number(e.target.value) || 0) })}
        />
      </div>
      <div className="transfer-form__checks">
        <Check
          label="CREATE TABLE"
          checked={o.includeCreate ?? true}
          disabled={disabled}
          onChange={(includeCreate) => set({ ...o, includeCreate })}
        />
        <Check
          label="DROP TABLE first"
          checked={o.includeDrop ?? false}
          disabled={disabled}
          onChange={(includeDrop) => set({ ...o, includeDrop })}
        />
      </div>
    </div>
  );
}

export function WriteOptionsForm({
  options,
  onChange,
  disabled = false,
  defaultTable,
}: {
  options: WriteOptions;
  onChange: (next: WriteOptions) => void;
  disabled?: boolean;
  /** The table name a SQL dump uses unless changed. */
  defaultTable: string;
}) {
  switch (options.format) {
    case "csv":
      return <CsvForm o={options} set={onChange} disabled={disabled} />;
    case "json":
      return <JsonForm o={options} set={onChange} disabled={disabled} />;
    case "xlsx":
      return <XlsxForm o={options} set={onChange} disabled={disabled} />;
    case "parquet":
      return <ParquetForm o={options} set={onChange} disabled={disabled} />;
    case "sql_dump":
      return (
        <DumpForm o={options} set={onChange} disabled={disabled} defaultTable={defaultTable} />
      );
  }
}
