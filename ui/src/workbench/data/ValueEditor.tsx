/**
 * Typed value editors shared by the grid's cell overlay, the form view and the filter builder:
 * text, numbers, booleans, date / time, JSON, enums and bytes, plus NULL and DEFAULT toggles.
 * Everything is edited as text in the core's grammar (docs/m3-contracts.md §5); NULL is a
 * separate state, never an empty string.
 */
import { type KeyboardEvent, useEffect, useRef } from "react";
import type { CellInput, InputKind } from "../../api";
import { formatJson, inputProblem, nowText, placeholderFor } from "./valueInput";

export interface TypedInputProps {
  kind: InputKind;
  text: string;
  onChange: (text: string) => void;
  label: string;
  disabled?: boolean | undefined;
  onKeyDown?: ((event: KeyboardEvent<HTMLElement>) => void) | undefined;
  /** Multi-line editors (JSON, long text) grow; single-line ones stay one row. */
  compact?: boolean | undefined;
  testId?: string | undefined;
}

/** One typed text input (no NULL state). */
export function TypedInput({
  kind,
  text,
  onChange,
  label,
  disabled,
  onKeyDown,
  compact,
  testId,
}: TypedInputProps) {
  const common = {
    "aria-label": label,
    disabled,
    onKeyDown,
    ...(testId ? { "data-testid": testId } : {}),
  };
  if (kind.type === "boolean") {
    const value = /^(t|true|1|yes)$/i.test(text)
      ? "true"
      : /^(f|false|0|no)$/i.test(text)
        ? "false"
        : "";
    return (
      <select
        {...common}
        className="value-input value-input--boolean"
        value={value}
        onChange={(e) => onChange(e.target.value)}
      >
        {value === "" ? <option value="">Choose…</option> : null}
        <option value="true">true</option>
        <option value="false">false</option>
      </select>
    );
  }
  if (kind.type === "enum" && kind.labels.length > 0) {
    return (
      <select
        {...common}
        className="value-input value-input--enum"
        value={text}
        onChange={(e) => onChange(e.target.value)}
      >
        {!kind.labels.includes(text) ? <option value={text}>{text || "Choose…"}</option> : null}
        {kind.labels.map((l) => (
          <option key={l} value={l}>
            {l}
          </option>
        ))}
      </select>
    );
  }
  if (kind.type === "json" && !compact) {
    return (
      <textarea
        {...common}
        className="value-input value-input--json"
        spellCheck={false}
        rows={Math.min(12, Math.max(3, text.split("\n").length))}
        value={text}
        onChange={(e) => onChange(e.target.value)}
      />
    );
  }
  const numeric = kind.type === "integer" || kind.type === "decimal" || kind.type === "float";
  return (
    <input
      {...common}
      type="text"
      className={`value-input value-input--${kind.type}`}
      inputMode={numeric ? "decimal" : undefined}
      spellCheck={false}
      placeholder={placeholderFor(kind)}
      value={text}
      onChange={(e) => onChange(e.target.value)}
    />
  );
}

export interface ValueEditorProps {
  kind: InputKind;
  value: CellInput;
  onChange: (value: CellInput) => void;
  label: string;
  /** Offers the NULL toggle. */
  nullable: boolean;
  /** Offers DEFAULT (new rows of columns with a default). */
  allowDefault?: boolean | undefined;
  disabled?: boolean | undefined;
  autoFocus?: boolean | undefined;
  onKeyDown?: ((event: KeyboardEvent<HTMLElement>) => void) | undefined;
}

/** A typed editor for one cell: the value, or NULL / DEFAULT. */
export function ValueEditor({
  kind,
  value,
  onChange,
  label,
  nullable,
  allowDefault,
  disabled,
  autoFocus,
  onKeyDown,
}: ValueEditorProps) {
  const text = value.type === "text" ? value.text : "";
  const problem = value.type === "text" && text !== "" ? inputProblem(kind, text) : null;
  const now = nowText(kind);
  const root = useRef<HTMLDivElement>(null);
  // Focus the editor once when it opens (the grid's cell overlay).
  useEffect(() => {
    if (!autoFocus) return;
    root.current?.querySelector<HTMLElement>("input, select, textarea, button")?.focus();
  }, [autoFocus]);
  return (
    <div className="value-editor" ref={root}>
      {value.type === "text" ? (
        <TypedInput
          kind={kind}
          text={text}
          label={label}
          disabled={disabled}
          onKeyDown={onKeyDown}
          onChange={(t) => onChange({ type: "text", text: t })}
        />
      ) : (
        <button
          type="button"
          className="value-editor__state"
          disabled={disabled}
          onKeyDown={onKeyDown}
          aria-label={`${label}: ${value.type === "null" ? "NULL" : "DEFAULT"}, click to type a value`}
          onClick={() => onChange({ type: "text", text: "" })}
        >
          {value.type === "null" ? "NULL" : "DEFAULT"}
        </button>
      )}
      <div className="value-editor__actions">
        {nullable ? (
          <label className="checkbox value-editor__toggle">
            <input
              type="checkbox"
              aria-label={`${label} is NULL`}
              checked={value.type === "null"}
              disabled={disabled}
              onChange={(e) =>
                onChange(e.target.checked ? { type: "null" } : { type: "text", text: "" })
              }
            />
            NULL
          </label>
        ) : null}
        {allowDefault ? (
          <label className="checkbox value-editor__toggle">
            <input
              type="checkbox"
              aria-label={`${label} uses the default`}
              checked={value.type === "default"}
              disabled={disabled}
              onChange={(e) =>
                onChange(e.target.checked ? { type: "default" } : { type: "text", text: "" })
              }
            />
            DEFAULT
          </label>
        ) : null}
        {now && value.type === "text" ? (
          <button
            type="button"
            className="button button--small"
            disabled={disabled}
            onClick={() => onChange({ type: "text", text: now })}
          >
            Now
          </button>
        ) : null}
        {kind.type === "json" && value.type === "text" ? (
          <button
            type="button"
            className="button button--small"
            disabled={disabled || problem !== null}
            onClick={() => onChange({ type: "text", text: formatJson(text) })}
          >
            Format
          </button>
        ) : null}
      </div>
      {problem ? (
        <p className="value-editor__hint" role="note">
          {problem}
        </p>
      ) : null}
    </div>
  );
}
