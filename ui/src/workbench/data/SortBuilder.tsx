/**
 * The sort builder: an ordered list of sort keys (column, direction, where NULLs go). When the
 * engine has no native `NULLS FIRST / LAST`, the core emulates it; the builder says so.
 */
import { type FilterColumn, move, newSortKey, type SortDraftKey } from "./filterModel";

export interface SortBuilderProps {
  keys: readonly SortDraftKey[];
  columns: readonly FilterColumn[];
  onChange: (keys: SortDraftKey[]) => void;
  /** The engine orders NULLs natively (else the core emulates it). */
  nullsNative: boolean;
  disabled?: boolean | undefined;
}

export function SortBuilder({ keys, columns, onChange, nullsNative, disabled }: SortBuilderProps) {
  const used = new Set(keys.map((k) => k.column));
  const free = columns.find((c) => !used.has(c.name))?.name ?? "";
  const set = (index: number, patch: Partial<SortDraftKey>) =>
    onChange(keys.map((k, i) => (i === index ? { ...k, ...patch } : k)));
  return (
    <fieldset className="sort" data-testid="sort-builder" aria-label="Sort">
      {keys.length === 0 ? <p className="filter__empty">Table order (no sort).</p> : null}
      <ol className="sort__keys">
        {keys.map((key, index) => (
          <li key={key.id} className="sort__key">
            <span className="sort__index">{index + 1}</span>
            <select
              aria-label={`Sort column ${index + 1}`}
              value={key.column}
              disabled={disabled}
              onChange={(e) => set(index, { column: e.target.value })}
            >
              {key.column === "" ? <option value="">Column…</option> : null}
              {columns.map((c) => (
                <option key={c.name} value={c.name}>
                  {c.name}
                </option>
              ))}
            </select>
            <select
              aria-label={`Direction ${index + 1}`}
              value={key.descending ? "desc" : "asc"}
              disabled={disabled}
              onChange={(e) => set(index, { descending: e.target.value === "desc" })}
            >
              <option value="asc">Ascending</option>
              <option value="desc">Descending</option>
            </select>
            <select
              aria-label={`NULLs ${index + 1}`}
              value={key.nulls}
              disabled={disabled}
              title={nullsNative ? undefined : "Emulated: this database has no NULLS FIRST/LAST"}
              onChange={(e) => set(index, { nulls: e.target.value as SortDraftKey["nulls"] })}
            >
              <option value="default">NULLs: default</option>
              <option value="first">NULLs first</option>
              <option value="last">NULLs last</option>
            </select>
            <button
              type="button"
              className="button button--small"
              aria-label={`Move sort key ${index + 1} up`}
              disabled={disabled || index === 0}
              onClick={() => onChange(move(keys, index, -1))}
            >
              ↑
            </button>
            <button
              type="button"
              className="button button--small"
              aria-label={`Move sort key ${index + 1} down`}
              disabled={disabled || index === keys.length - 1}
              onClick={() => onChange(move(keys, index, 1))}
            >
              ↓
            </button>
            <button
              type="button"
              className="button button--small"
              aria-label={`Remove sort key ${index + 1}`}
              disabled={disabled}
              onClick={() => onChange(keys.filter((_, i) => i !== index))}
            >
              ×
            </button>
          </li>
        ))}
      </ol>
      <button
        type="button"
        className="button button--small"
        disabled={disabled || columns.length === 0}
        onClick={() => onChange([...keys, newSortKey(free)])}
      >
        + Sort key
      </button>
      {!nullsNative && keys.some((k) => k.nulls !== "default") ? (
        <p className="field__hint">NULL ordering is emulated on this database.</p>
      ) : null}
    </fieldset>
  );
}
