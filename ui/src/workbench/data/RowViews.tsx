/**
 * The form view (one row, one typed editor per column) and the JSON view (the current row or
 * the selected rows) of a table-data tab. Values include pending edits; the panel supplies
 * them.
 */
import {
  Braces,
  ChevronLeft,
  ChevronRight,
  CircleAlert,
  Copy,
  KeyRound,
  LoaderCircle,
} from "lucide-react";
import type { CellInput, EditColumn } from "../../api";
import { ICON, ICON_SM } from "../../components/icons";
import { ValueEditor } from "./ValueEditor";

export interface FormFieldView {
  column: EditColumn;
  value: CellInput;
  /** The value is a pending edit. */
  pending: boolean;
  editable: boolean;
  readOnlyReason?: string | undefined;
}

export interface RowFormProps {
  title: string;
  status?: string | undefined;
  fields: FormFieldView[] | null;
  error?: string | null | undefined;
  /** New rows may use column defaults. */
  allowDefault: boolean;
  onChange: (column: string, input: CellInput) => void;
  onPrev?: (() => void) | undefined;
  onNext?: (() => void) | undefined;
}

export function RowForm({
  title,
  status,
  fields,
  error,
  allowDefault,
  onChange,
  onPrev,
  onNext,
}: RowFormProps) {
  return (
    <section className="row-form" aria-label="Form view" data-testid="form-view">
      <div className="row-form__head">
        <div className="row-form__nav">
          <button
            type="button"
            className="icon-button"
            aria-label="Previous row"
            title="Previous row"
            disabled={!onPrev}
            onClick={onPrev}
          >
            <ChevronLeft {...ICON} />
          </button>
          <button
            type="button"
            className="icon-button"
            aria-label="Next row"
            title="Next row"
            disabled={!onNext}
            onClick={onNext}
          >
            <ChevronRight {...ICON} />
          </button>
        </div>
        <h3 className="row-form__title">{title}</h3>
        {status ? <span className={`row-badge row-badge--${status}`}>{status}</span> : null}
      </div>
      {error ? (
        <p className="row-form__error" role="alert">
          <CircleAlert {...ICON_SM} />
          {error}
        </p>
      ) : fields === null ? (
        <p className="row-views__loading" role="status">
          <LoaderCircle {...ICON_SM} className="spin" />
          Loading…
        </p>
      ) : fields.length === 0 ? (
        <p className="results__empty">No row selected.</p>
      ) : (
        <div className="row-form__fields">
          {fields.map((f) => (
            <div
              key={f.column.name}
              className={`row-form__field${f.pending ? " row-form__field--pending" : ""}`}
            >
              <div className="row-form__label">
                <span className="row-form__name">
                  {f.column.key ? (
                    <span className="row-form__key" title="Key column">
                      <KeyRound {...ICON_SM} />
                      <span className="visually-hidden"> key</span>
                    </span>
                  ) : null}
                  {f.column.name}
                </span>
                <span className="row-form__type">
                  {f.column.nativeType}
                  {f.column.nullable ? "" : " · not null"}
                </span>
              </div>
              <ValueEditor
                kind={f.column.input}
                dataType={f.column.dataType}
                value={f.value}
                label={f.column.name}
                nullable={f.column.nullable}
                allowDefault={allowDefault && f.column.hasDefault}
                disabled={!f.editable}
                onChange={(input) => onChange(f.column.name, input)}
              />
              {!f.editable && f.readOnlyReason ? (
                <p className="field__hint">{f.readOnlyReason}</p>
              ) : null}
            </div>
          ))}
        </div>
      )}
    </section>
  );
}

export interface JsonViewProps {
  title: string;
  json: string | null;
  error?: string | null | undefined;
  note?: string | undefined;
}

export function JsonView({ title, json, error, note }: JsonViewProps) {
  return (
    <section className="json-view" aria-label="JSON view">
      <div className="row-form__head">
        <Braces {...ICON_SM} className="row-form__icon" />
        <h3 className="row-form__title">{title}</h3>
        <button
          type="button"
          className="button button--ghost button--small row-form__end"
          disabled={json === null}
          onClick={() => {
            if (json !== null) void navigator.clipboard?.writeText(json).catch(() => {});
          }}
        >
          <Copy {...ICON_SM} />
          Copy
        </button>
      </div>
      {note ? <p className="field__hint">{note}</p> : null}
      {error ? (
        <p className="row-form__error" role="alert">
          <CircleAlert {...ICON_SM} />
          {error}
        </p>
      ) : json === null ? (
        <p className="row-views__loading" role="status">
          <LoaderCircle {...ICON_SM} className="spin" />
          Loading…
        </p>
      ) : (
        <pre className="json-view__text" data-testid="json-view">
          {json}
        </pre>
      )}
    </section>
  );
}
