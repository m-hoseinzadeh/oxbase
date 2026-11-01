/**
 * The table designer's editors: columns (with primary key, defaults, identity, generated
 * columns), indexes, foreign keys, checks and table options. Each editor changes the
 * `TableDesign` it is given and keeps every item's `origin`; what the engine cannot do
 * (`DesignerCapabilities`) is disabled with the reason as its title.
 */
import { Fragment, useId, useState } from "react";
import type {
  CheckDef,
  DesignColumn,
  DesignerCapabilities,
  DesignIssue,
  DesignItem,
  Engine,
  FkAction,
  ForeignKeyDef,
  IndexColumn,
  IndexDef,
  ServerVersion,
  TableDesign,
} from "../../api";
import {
  allowed,
  boolExtra,
  columnLockReason,
  type DefaultMode,
  type DesignTraits,
  defaultMode,
  dependentsOfColumn,
  designTraits,
  dropColumnRefs,
  EXTRA,
  guessDataType,
  indexLockReason,
  issuesAt,
  moveItem,
  namesExtra,
  newColumn,
  newForeignKey,
  newIndex,
  renameColumn,
  splitNames,
  supportHint,
  textExtra,
  typeSuggestions,
  withDefault,
  withExtra,
} from "./designModel";

export interface EditorProps {
  design: TableDesign;
  onChange: (design: TableDesign) => void;
  caps: DesignerCapabilities;
  issues: readonly DesignIssue[];
  engine: Engine | undefined;
  /** The server's version (some types depend on it). */
  serverVersion?: ServerVersion | undefined;
  /** Designing a new table (everything is allowed that `createTable` allows). */
  isNew: boolean;
  disabled: boolean;
}

function IssueText({ issues }: { issues: DesignIssue[] }) {
  if (issues.length === 0) return null;
  return (
    <span className="designer__issue-inline">
      {issues.map((i) => (
        <span
          key={`${i.path}:${i.message}`}
          className={`designer__issue designer__issue--${i.severity}`}
        >
          {i.message}
        </span>
      ))}
    </span>
  );
}

function issueClass(issues: DesignIssue[]): string {
  if (issues.some((i) => i.severity === "error")) return " designer__row--error";
  if (issues.length > 0) return " designer__row--warning";
  return "";
}

const FK_LABEL: Record<FkAction, string> = {
  no_action: "NO ACTION",
  restrict: "RESTRICT",
  cascade: "CASCADE",
  set_null: "SET NULL",
  set_default: "SET DEFAULT",
};

// --- columns ----------------------------------------------------------------------------

export function ColumnsEditor({
  design,
  onChange,
  caps,
  issues,
  engine,
  serverVersion,
  isNew,
  disabled,
}: EditorProps) {
  const listId = useId();
  const traits = designTraits(engine);
  const [details, setDetails] = useState<number | null>(null);
  const [removedNote, setRemovedNote] = useState<string | null>(null);
  const pk = new Set(design.primaryKey?.columns ?? []);
  const max = caps.maxIdentifierLength ?? undefined;
  const canAdd = isNew ? caps.createTable : allowed(caps.addColumn);
  const canReorder = isNew || allowed(caps.reorderColumns);
  const canPk = isNew || allowed(caps.changePrimaryKey);

  /** Replaces column `index` (its name is changed only through `commitName`). */
  function setColumn(index: number, next: DesignColumn) {
    onChange({ ...design, columns: design.columns.map((c, i) => (i === index ? next : c)) });
  }

  /** A committed rename (blur or Enter): references follow it once, from the old name. */
  function commitName(index: number, name: string): string | null {
    const result = renameColumn(design, index, name);
    if ("error" in result) return result.error;
    if (result.design !== design) onChange(result.design);
    return null;
  }

  function setColumnDef(index: number, patch: Partial<DesignColumn["column"]>) {
    const col = design.columns[index];
    if (col) setColumn(index, { ...col, column: { ...col.column, ...patch } });
  }

  function togglePk(name: string, on: boolean) {
    const columns = design.primaryKey?.columns ?? [];
    const next = on ? [...columns, name] : columns.filter((c) => c !== name);
    const d: TableDesign = { ...design };
    if (next.length === 0) delete d.primaryKey;
    else d.primaryKey = { ...(design.primaryKey ?? {}), columns: next };
    // Primary key columns are NOT NULL.
    if (on) {
      d.columns = d.columns.map((c) =>
        c.column.name === name ? { ...c, column: { ...c.column, nullable: false } } : c,
      );
    }
    onChange(d);
  }

  function remove(index: number) {
    const col = design.columns[index];
    if (!col) return;
    const dependents = col.column.name ? dependentsOfColumn(design, col.column.name) : [];
    setRemovedNote(
      dependents.length > 0
        ? `Removing ${col.column.name} also removed ${dependents.join(", ")}.`
        : null,
    );
    const d = dropColumnRefs(
      { ...design, columns: design.columns.filter((_, i) => i !== index) },
      col.column.name,
    );
    onChange(d);
    setDetails(null);
  }

  return (
    <div className="designer__section">
      <datalist id={listId}>
        {typeSuggestions(engine, serverVersion).map((t) => (
          <option key={t} value={t} />
        ))}
      </datalist>
      <table className="designer__table" aria-label="Columns">
        <thead>
          <tr>
            <th aria-label="Order" />
            <th>Name</th>
            <th>Type</th>
            <th>Not null</th>
            <th>Key</th>
            <th>Default</th>
            <th>Comment</th>
            <th aria-label="Actions" />
          </tr>
        </thead>
        <tbody>
          {design.columns.map((col, index) => {
            const existing = !isNew && col.origin !== undefined;
            const rowIssues = issuesAt(issues, `columns[${index}]`);
            const mode = defaultMode(col);
            const defaultText =
              mode === "value" ? (col.defaultLiteral ?? "") : (col.column.default ?? "");
            const nameHint = existing
              ? supportHint(caps.renameColumn, "Renaming a column")
              : undefined;
            const lockReason = existing ? columnLockReason(col.column) : null;
            const typeHint = existing
              ? (lockReason ?? supportHint(caps.alterColumnType, "Changing the type"))
              : undefined;
            const nullHint = existing
              ? supportHint(caps.alterColumnNullability, "Changing NOT NULL")
              : undefined;
            const defaultHint = existing
              ? supportHint(caps.alterColumnDefault, "Changing the default")
              : undefined;
            const commentHint = supportHint(caps.columnComment, "Column comments");
            const dropHint = existing
              ? supportHint(caps.dropColumn, "Dropping a column")
              : undefined;
            const can = (support: Parameters<typeof allowed>[0]) => !existing || allowed(support);
            const key = col.origin === undefined ? `new-${index}` : `o${col.origin}`;
            return (
              <Fragment key={key}>
                <tr
                  className={`designer__row${issueClass(rowIssues)}`}
                  data-testid={`column-row-${index}`}
                >
                  <td className="designer__order">
                    <button
                      type="button"
                      className="button button--small"
                      aria-label={`Move ${col.column.name || "column"} up`}
                      disabled={disabled || !canReorder || index === 0}
                      title={
                        canReorder
                          ? undefined
                          : supportHint(caps.reorderColumns, "Reordering columns")
                      }
                      onClick={() =>
                        onChange({ ...design, columns: moveItem(design.columns, index, -1) })
                      }
                    >
                      ↑
                    </button>
                    <button
                      type="button"
                      className="button button--small"
                      aria-label={`Move ${col.column.name || "column"} down`}
                      disabled={disabled || !canReorder || index === design.columns.length - 1}
                      onClick={() =>
                        onChange({ ...design, columns: moveItem(design.columns, index, 1) })
                      }
                    >
                      ↓
                    </button>
                  </td>
                  <td>
                    <ColumnNameInput
                      label={`Column ${index + 1} name`}
                      value={col.column.name}
                      maxLength={max}
                      disabled={disabled || !can(caps.renameColumn)}
                      title={nameHint}
                      onCommit={(name) => commitName(index, name)}
                    />
                  </td>
                  <td>
                    <input
                      type="text"
                      list={listId}
                      aria-label={`Column ${index + 1} type`}
                      value={col.column.native_type}
                      disabled={disabled || !can(caps.alterColumnType) || lockReason !== null}
                      title={typeHint}
                      spellCheck={false}
                      onChange={(e) =>
                        setColumnDef(index, {
                          native_type: e.target.value,
                          data_type: guessDataType(e.target.value, engine),
                        })
                      }
                    />
                  </td>
                  <td className="designer__center">
                    <input
                      type="checkbox"
                      aria-label={`Column ${index + 1} not null`}
                      checked={!col.column.nullable}
                      disabled={
                        disabled || !can(caps.alterColumnNullability) || pk.has(col.column.name)
                      }
                      title={nullHint}
                      onChange={(e) => setColumnDef(index, { nullable: !e.target.checked })}
                    />
                  </td>
                  <td className="designer__center">
                    <input
                      type="checkbox"
                      aria-label={`Column ${index + 1} primary key`}
                      checked={pk.has(col.column.name)}
                      disabled={disabled || !canPk || col.column.name === ""}
                      title={
                        canPk
                          ? undefined
                          : supportHint(caps.changePrimaryKey, "Changing the primary key")
                      }
                      onChange={(e) => togglePk(col.column.name, e.target.checked)}
                    />
                  </td>
                  <td className="designer__default">
                    <select
                      aria-label={`Column ${index + 1} default kind`}
                      value={mode}
                      disabled={
                        disabled ||
                        !can(caps.alterColumnDefault) ||
                        col.column.generated !== undefined
                      }
                      title={defaultHint}
                      onChange={(e) =>
                        setColumn(
                          index,
                          withDefault(col, e.target.value as DefaultMode, defaultText),
                        )
                      }
                    >
                      <option value="none">No default</option>
                      <option value="value">Value</option>
                      <option value="expression">Expression</option>
                    </select>
                    {mode !== "none" ? (
                      <input
                        type="text"
                        aria-label={`Column ${index + 1} default`}
                        className={mode === "expression" ? "designer__expr" : undefined}
                        value={defaultText}
                        placeholder={
                          mode === "value" ? "plain value (quoted for you)" : "SQL expression"
                        }
                        disabled={disabled || !can(caps.alterColumnDefault)}
                        spellCheck={false}
                        onChange={(e) => setColumn(index, withDefault(col, mode, e.target.value))}
                      />
                    ) : null}
                  </td>
                  <td>
                    <input
                      type="text"
                      aria-label={`Column ${index + 1} comment`}
                      value={col.column.comment ?? ""}
                      disabled={disabled || !allowed(caps.columnComment)}
                      title={commentHint}
                      onChange={(e) => {
                        const { comment: _c, ...rest } = col.column;
                        setColumn(index, {
                          ...col,
                          column: e.target.value ? { ...rest, comment: e.target.value } : rest,
                        });
                      }}
                    />
                  </td>
                  <td className="designer__actions">
                    <button
                      type="button"
                      className="button button--small"
                      aria-expanded={details === index}
                      aria-label={`More options of column ${index + 1}`}
                      disabled={disabled}
                      onClick={() => setDetails(details === index ? null : index)}
                    >
                      …
                    </button>
                    <button
                      type="button"
                      className="button button--small"
                      aria-label={`Remove column ${index + 1}`}
                      disabled={disabled || !can(caps.dropColumn)}
                      title={dropHint}
                      onClick={() => remove(index)}
                    >
                      ×
                    </button>
                  </td>
                </tr>
                {rowIssues.length > 0 ? (
                  <tr className="designer__issues-row">
                    <td />
                    <td colSpan={7}>
                      <IssueText issues={rowIssues} />
                    </td>
                  </tr>
                ) : null}
                {details === index ? (
                  <tr className="designer__details-row">
                    <td />
                    <td colSpan={7}>
                      <ColumnDetails
                        col={col}
                        index={index}
                        caps={caps}
                        traits={traits}
                        otherIdentity={
                          design.columns.find((c, i) => i !== index && c.column.identity)?.column
                            .name ?? null
                        }
                        existing={existing}
                        disabled={disabled}
                        onChange={(next) => setColumn(index, next)}
                      />
                    </td>
                  </tr>
                ) : null}
              </Fragment>
            );
          })}
        </tbody>
      </table>
      {removedNote ? (
        <p className="designer__note" role="status" data-testid="column-removed-note">
          {removedNote}
        </p>
      ) : null}
      <div className="designer__add">
        <button
          type="button"
          className="button"
          disabled={disabled || !canAdd}
          title={canAdd ? undefined : supportHint(caps.addColumn, "Adding a column")}
          onClick={() => onChange({ ...design, columns: [...design.columns, newColumn()] })}
        >
          + Column
        </button>
      </div>
    </div>
  );
}

/**
 * A column name edited as a draft and committed on blur or Enter (Escape drops the draft), so
 * references are renamed once from the committed name and never from a half-typed one. A name
 * that is empty or taken stays as an invalid draft with the reason.
 */
function ColumnNameInput({
  label,
  value,
  maxLength,
  disabled,
  title,
  onCommit,
}: {
  label: string;
  value: string;
  maxLength: number | undefined;
  disabled: boolean;
  title: string | undefined;
  /** Commits the name; returns why it was refused, or `null`. */
  onCommit: (name: string) => string | null;
}) {
  const [draft, setDraft] = useState<string | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const errorId = useId();
  function commit() {
    if (draft === null) return;
    if (draft === value) {
      setDraft(null);
      setProblem(null);
      return;
    }
    const refused = onCommit(draft);
    setProblem(refused);
    if (!refused) setDraft(null);
  }
  return (
    <>
      <input
        type="text"
        aria-label={label}
        value={draft ?? value}
        maxLength={maxLength}
        disabled={disabled}
        title={problem ?? title}
        aria-invalid={problem ? true : undefined}
        aria-describedby={problem ? errorId : undefined}
        spellCheck={false}
        onChange={(e) => {
          setDraft(e.target.value);
          setProblem(null);
        }}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") commit();
          else if (e.key === "Escape") {
            setDraft(null);
            setProblem(null);
          }
        }}
      />
      {problem ? (
        <span id={errorId} className="designer__issue designer__issue--error" role="alert">
          {problem}
        </span>
      ) : null}
    </>
  );
}

function ColumnDetails({
  col,
  index,
  caps,
  traits,
  otherIdentity,
  existing,
  disabled,
  onChange,
}: {
  col: DesignColumn;
  index: number;
  caps: DesignerCapabilities;
  traits: DesignTraits;
  /** Another column of the table that has identity (for engines with one per table). */
  otherIdentity: string | null;
  existing: boolean;
  disabled: boolean;
  onChange: (next: DesignColumn) => void;
}) {
  const def = col.column;
  const set = (patch: Partial<DesignColumn["column"]>) =>
    onChange({ ...col, column: { ...def, ...patch } });
  const identity = def.identity ? (def.identity.always ? "always" : "default") : "none";
  const canGenerated = caps.generatedStored || caps.generatedVirtual;
  // Auto increment and identity need the engine's support; on a stored column they are a
  // change of the column's definition, so the engine must also be able to alter it.
  const autoHint = !caps.autoIncrement
    ? "Auto increment is not supported by this database"
    : existing
      ? supportHint(caps.alterColumnType, "Changing auto increment")
      : undefined;
  const canAuto = caps.autoIncrement && (!existing || allowed(caps.alterColumnType));
  const identityHint = !caps.identity
    ? "Identity columns are not supported by this database"
    : existing
      ? supportHint(caps.alterColumnType, "Changing identity")
      : undefined;
  const canIdentity = caps.identity && (!existing || allowed(caps.alterColumnType));
  const seedIncrement = traits.identityStyle === "seedIncrement";
  return (
    <div className="designer__details">
      {seedIncrement && (caps.identity || def.identity) ? (
        <SeedIncrementIdentity
          col={col}
          index={index}
          caps={caps}
          traits={traits}
          otherIdentity={otherIdentity}
          existing={existing}
          disabled={disabled}
          onChange={onChange}
        />
      ) : null}
      {!seedIncrement && (caps.autoIncrement || def.auto_increment) ? (
        <label className="checkbox" title={autoHint}>
          <input
            type="checkbox"
            aria-label={`Column ${index + 1} auto increment`}
            checked={def.auto_increment}
            disabled={disabled || !canAuto}
            onChange={(e) => set({ auto_increment: e.target.checked })}
          />
          Auto increment
        </label>
      ) : null}
      {!seedIncrement && (caps.identity || def.identity) ? (
        <label className="designer__inline" title={identityHint}>
          Identity
          <select
            aria-label={`Column ${index + 1} identity`}
            value={identity}
            disabled={disabled || !canIdentity}
            onChange={(e) => {
              const v = e.target.value;
              const { identity: _i, ...rest } = def;
              onChange({
                ...col,
                column: v === "none" ? rest : { ...rest, identity: { always: v === "always" } },
              });
            }}
          >
            <option value="none">none</option>
            <option value="default">GENERATED BY DEFAULT</option>
            <option value="always">GENERATED ALWAYS</option>
          </select>
        </label>
      ) : null}
      <label className="designer__inline">
        Collation
        <input
          type="text"
          aria-label={`Column ${index + 1} collation`}
          value={def.collation ?? ""}
          disabled={
            disabled ||
            (existing ? !allowed(caps.columnCollation) : caps.columnCollation === "unsupported")
          }
          title={supportHint(caps.columnCollation, "Collations")}
          onChange={(e) => {
            const { collation: _c, ...rest } = def;
            onChange({
              ...col,
              column: e.target.value ? { ...rest, collation: e.target.value } : rest,
            });
          }}
        />
      </label>
      {canGenerated ? (
        <label className="designer__inline designer__inline--wide">
          Generated as
          <input
            type="text"
            className="designer__expr"
            aria-label={`Column ${index + 1} generated expression`}
            value={def.generated?.expression ?? ""}
            placeholder="SQL expression (leave empty for a plain column)"
            disabled={disabled || existing}
            title={
              existing ? "A generated expression is set when the column is created" : undefined
            }
            spellCheck={false}
            onChange={(e) => {
              const { generated: _g, ...rest } = def;
              const stored = def.generated?.stored ?? !caps.generatedVirtual;
              onChange({
                ...col,
                column: e.target.value
                  ? { ...rest, generated: { expression: e.target.value, stored } }
                  : rest,
              });
            }}
          />
          {def.generated ? (
            <label className="checkbox">
              <input
                type="checkbox"
                checked={def.generated.stored}
                disabled={disabled || existing || !(caps.generatedStored && caps.generatedVirtual)}
                onChange={(e) =>
                  def.generated &&
                  set({ generated: { ...def.generated, stored: e.target.checked } })
                }
              />
              {traits.storedLabel}
            </label>
          ) : null}
        </label>
      ) : null}
    </div>
  );
}

const WHOLE_NUMBER = /^[+-]?\d+$/;

/**
 * `IDENTITY(seed, increment)` of a column (SQL Server): set when the column is created, never
 * added to, removed from or changed on an existing column, and on at most one column.
 */
function SeedIncrementIdentity({
  col,
  index,
  caps,
  traits,
  otherIdentity,
  existing,
  disabled,
  onChange,
}: {
  col: DesignColumn;
  index: number;
  caps: DesignerCapabilities;
  traits: DesignTraits;
  otherIdentity: string | null;
  existing: boolean;
  disabled: boolean;
  onChange: (next: DesignColumn) => void;
}) {
  const def = col.column;
  const identity = def.identity;
  const lockedByExisting = existing && traits.identityOnNewColumnsOnly;
  const lockedByOther = !identity && traits.singleIdentity && otherIdentity !== null;
  const hint = !caps.identity
    ? "Identity columns are not supported by this database"
    : lockedByExisting
      ? "IDENTITY cannot be added to, removed from or changed on an existing column: add a new column or recreate the table."
      : lockedByOther
        ? `The table already has an identity column (${otherIdentity || "unnamed"}).`
        : undefined;
  const locked = disabled || !caps.identity || lockedByExisting;
  const seed = identity?.start ?? "1";
  const increment = identity?.increment ?? "1";
  const problem = !identity
    ? null
    : !WHOLE_NUMBER.test(seed.trim())
      ? "The seed must be a whole number."
      : !WHOLE_NUMBER.test(increment.trim()) || /^[+-]?0+$/.test(increment.trim())
        ? "The increment must be a whole number other than 0."
        : null;

  function toggle(on: boolean) {
    if (on) {
      // Identity columns are NOT NULL and have no default.
      const { identity: _i, default: _d, ...rest } = def;
      const { defaultLiteral: _l, ...colRest } = col;
      onChange({
        ...colRest,
        column: {
          ...rest,
          nullable: false,
          auto_increment: true,
          identity: { always: true, start: "1", increment: "1" },
        },
      });
    } else {
      const { identity: _i, ...rest } = def;
      onChange({ ...col, column: { ...rest, auto_increment: false } });
    }
  }

  function setPart(part: "start" | "increment", text: string) {
    if (!identity) return;
    onChange({ ...col, column: { ...def, identity: { ...identity, [part]: text } } });
  }

  return (
    <span className="designer__inline designer__identity" title={hint}>
      <label className="checkbox">
        <input
          type="checkbox"
          aria-label={`Column ${index + 1} identity`}
          checked={identity !== undefined}
          disabled={locked || lockedByOther}
          onChange={(e) => toggle(e.target.checked)}
        />
        Identity
      </label>
      {identity ? (
        <>
          <label className="designer__inline">
            Seed
            <input
              type="text"
              inputMode="numeric"
              className="designer__short"
              aria-label={`Column ${index + 1} identity seed`}
              value={seed}
              disabled={locked}
              spellCheck={false}
              onChange={(e) => setPart("start", e.target.value)}
            />
          </label>
          <label className="designer__inline">
            Increment
            <input
              type="text"
              inputMode="numeric"
              className="designer__short"
              aria-label={`Column ${index + 1} identity increment`}
              value={increment}
              disabled={locked}
              spellCheck={false}
              onChange={(e) => setPart("increment", e.target.value)}
            />
          </label>
        </>
      ) : null}
      {problem ? (
        <span className="designer__issue designer__issue--error" role="alert">
          {problem}
        </span>
      ) : null}
      {lockedByExisting && caps.identity ? (
        <span className="field__hint designer__identity-hint">{hint}</span>
      ) : null}
    </span>
  );
}

// --- indexes ----------------------------------------------------------------------------

function indexColumnsText(columns: readonly IndexColumn[]): string {
  return columns
    .map(
      (c) =>
        `${c.is_expression ? `(${c.name})` : c.name}${c.length ? `(${c.length})` : ""}${c.descending ? " DESC" : ""}`,
    )
    .join(", ");
}

/** Parses "a, b DESC, (lower(c)), d(10)" into index columns. */
export function parseIndexColumns(text: string): IndexColumn[] {
  const parts: string[] = [];
  let depth = 0;
  let current = "";
  for (const ch of text) {
    if (ch === "(") depth += 1;
    if (ch === ")") depth = Math.max(0, depth - 1);
    if (ch === "," && depth === 0) {
      parts.push(current);
      current = "";
    } else current += ch;
  }
  parts.push(current);
  const out: IndexColumn[] = [];
  for (const raw of parts) {
    let part = raw.trim();
    if (!part) continue;
    let descending = false;
    const dir = /\s+(asc|desc)$/i.exec(part);
    if (dir) {
      descending = dir[1]?.toLowerCase() === "desc";
      part = part.slice(0, dir.index).trim();
    }
    if (part.startsWith("(") && part.endsWith(")")) {
      out.push({ name: part.slice(1, -1).trim(), is_expression: true, descending });
      continue;
    }
    const len = /^(.*?)\((\d+)\)$/.exec(part);
    if (len?.[1] && len[2]) {
      out.push({ name: len[1].trim(), is_expression: false, descending, length: Number(len[2]) });
    } else {
      out.push({ name: part, is_expression: false, descending });
    }
  }
  return out;
}

/** A text field edited as a draft and parsed when it loses focus (or on Enter). */
function DraftInput({
  value,
  onCommit,
  label,
  disabled,
  title,
  placeholder,
}: {
  value: string;
  onCommit: (text: string) => void;
  label: string;
  disabled: boolean;
  title?: string | undefined;
  placeholder?: string | undefined;
}) {
  const [draft, setDraft] = useState<string | null>(null);
  return (
    <input
      type="text"
      aria-label={label}
      value={draft ?? value}
      disabled={disabled}
      title={title}
      placeholder={placeholder}
      spellCheck={false}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={() => {
        if (draft !== null && draft !== value) onCommit(draft);
        setDraft(null);
      }}
      onKeyDown={(e) => {
        if (e.key === "Enter" && draft !== null) {
          onCommit(draft);
          setDraft(null);
        }
      }}
    />
  );
}

/** `index` marked clustered or not (the `index_kind` extra follows when it is a plain kind). */
function withClustered(index: IndexDef, on: boolean): IndexDef {
  let extras = withExtra(index.extras, EXTRA.clustered, { type: "bool", value: on });
  const kind = textExtra(index.extras, EXTRA.indexKind);
  if (kind === "clustered" || kind === "nonclustered") {
    extras = withExtra(extras, EXTRA.indexKind, {
      type: "text",
      value: on ? "clustered" : "nonclustered",
    });
  }
  const { extras: _e, ...rest } = index;
  return { ...rest, extras };
}

export function IndexesEditor({
  design,
  onChange,
  caps,
  issues,
  engine,
  isNew,
  disabled,
}: EditorProps) {
  const traits = designTraits(engine);
  const canAdd = isNew || allowed(caps.addIndex);
  const canChange = (item: DesignItem<IndexDef>) =>
    isNew || item.origin === undefined || (allowed(caps.addIndex) && allowed(caps.dropIndex));
  const set = (index: number, def: IndexDef) =>
    onChange({
      ...design,
      indexes: design.indexes.map((it, i) => (i === index ? { ...it, def } : it)),
    });
  return (
    <div className="designer__section">
      {design.indexes.length === 0 ? <p className="filter__empty">No indexes.</p> : null}
      <table className="designer__table" aria-label="Indexes">
        {design.indexes.length > 0 ? (
          <thead>
            <tr>
              <th>Name</th>
              <th>Columns</th>
              <th>Unique</th>
              {caps.uniqueConstraints ? <th>Constraint</th> : null}
              {traits.clusteredIndexes ? <th>Clustered</th> : null}
              {traits.includedColumns ? <th>Include</th> : null}
              {caps.partialIndexes ? <th>Where</th> : null}
              <th aria-label="Actions" />
            </tr>
          </thead>
        ) : null}
        <tbody>
          {design.indexes.map((item, index) => {
            const rowIssues = issuesAt(issues, `indexes[${index}]`);
            const kindLock = indexLockReason(item.def, traits);
            const locked = disabled || !canChange(item) || kindLock !== null;
            const clustered = boolExtra(item.def.extras, EXTRA.clustered) ?? false;
            const columnCount =
              4 +
              (caps.uniqueConstraints ? 1 : 0) +
              (traits.clusteredIndexes ? 1 : 0) +
              (traits.includedColumns ? 1 : 0) +
              (caps.partialIndexes ? 1 : 0);
            const key = item.origin === undefined ? `new-${index}` : `o${item.origin}`;
            const hasDesc = item.def.columns.some((c) => c.descending);
            const hasExpr = item.def.columns.some((c) => c.is_expression);
            const hasLength = item.def.columns.some((c) => c.length !== undefined);
            const unsupportedParts = [
              hasDesc && !caps.descendingIndexes ? "descending columns" : null,
              hasExpr && !caps.expressionIndexes ? "expressions" : null,
              hasLength && !caps.indexPrefixLengths ? "prefix lengths" : null,
            ].filter(Boolean);
            return (
              <Fragment key={key}>
                <tr className={`designer__row${issueClass(rowIssues)}`}>
                  <td>
                    <input
                      type="text"
                      aria-label={`Index ${index + 1} name`}
                      value={item.def.name}
                      maxLength={caps.maxIdentifierLength ?? undefined}
                      disabled={locked}
                      onChange={(e) => set(index, { ...item.def, name: e.target.value })}
                    />
                  </td>
                  <td>
                    <DraftInput
                      label={`Index ${index + 1} columns`}
                      value={indexColumnsText(item.def.columns)}
                      disabled={locked}
                      placeholder={`a, b DESC${caps.expressionIndexes ? ", (lower(c))" : ""}${caps.indexPrefixLengths ? ", d(10)" : ""}`}
                      onCommit={(text) =>
                        set(index, { ...item.def, columns: parseIndexColumns(text) })
                      }
                    />
                  </td>
                  <td className="designer__center">
                    <input
                      type="checkbox"
                      aria-label={`Index ${index + 1} unique`}
                      checked={item.def.unique}
                      disabled={locked}
                      onChange={(e) =>
                        set(index, {
                          ...item.def,
                          unique: e.target.checked,
                          is_constraint: e.target.checked && item.def.is_constraint,
                        })
                      }
                    />
                  </td>
                  {caps.uniqueConstraints ? (
                    <td className="designer__center">
                      <input
                        type="checkbox"
                        aria-label={`Index ${index + 1} is a constraint`}
                        checked={item.def.is_constraint}
                        disabled={locked || !item.def.unique}
                        onChange={(e) =>
                          set(index, { ...item.def, is_constraint: e.target.checked })
                        }
                      />
                    </td>
                  ) : null}
                  {traits.clusteredIndexes ? (
                    <td className="designer__center">
                      <input
                        type="checkbox"
                        aria-label={`Index ${index + 1} clustered`}
                        checked={clustered}
                        disabled={locked}
                        title="A table has at most one clustered index (the primary key's, when it is clustered)."
                        onChange={(e) => set(index, withClustered(item.def, e.target.checked))}
                      />
                    </td>
                  ) : null}
                  {traits.includedColumns ? (
                    <td>
                      <DraftInput
                        label={`Index ${index + 1} included columns`}
                        value={namesExtra(item.def.extras, EXTRA.includeColumns).join(", ")}
                        disabled={locked || item.def.is_constraint || clustered}
                        title={
                          item.def.is_constraint
                            ? "A unique constraint has no included columns"
                            : clustered
                              ? "A clustered index already holds every column"
                              : undefined
                        }
                        placeholder="c, d"
                        onCommit={(text) =>
                          set(index, {
                            ...item.def,
                            // Always written (also empty), so removing them is a change.
                            extras: withExtra(item.def.extras, EXTRA.includeColumns, {
                              type: "json",
                              value: splitNames(text),
                            }),
                          })
                        }
                      />
                    </td>
                  ) : null}
                  {caps.partialIndexes ? (
                    <td>
                      <input
                        type="text"
                        className="designer__expr"
                        aria-label={`Index ${index + 1} predicate`}
                        value={item.def.predicate ?? ""}
                        placeholder="partial index condition"
                        disabled={locked || item.def.is_constraint}
                        spellCheck={false}
                        onChange={(e) => {
                          const { predicate: _p, ...rest } = item.def;
                          set(
                            index,
                            e.target.value ? { ...rest, predicate: e.target.value } : rest,
                          );
                        }}
                      />
                    </td>
                  ) : null}
                  <td className="designer__actions">
                    <button
                      type="button"
                      className="button button--small"
                      aria-label={`Remove index ${index + 1}`}
                      disabled={
                        disabled ||
                        (!isNew && item.origin !== undefined && !allowed(caps.dropIndex))
                      }
                      title={
                        item.origin !== undefined
                          ? supportHint(caps.dropIndex, "Dropping an index")
                          : undefined
                      }
                      onClick={() =>
                        onChange({
                          ...design,
                          indexes: design.indexes.filter((_, i) => i !== index),
                        })
                      }
                    >
                      ×
                    </button>
                  </td>
                </tr>
                {rowIssues.length > 0 || unsupportedParts.length > 0 || kindLock ? (
                  <tr className="designer__issues-row">
                    <td colSpan={columnCount}>
                      {kindLock ? (
                        <span className="designer__issue designer__issue--warning">{kindLock}</span>
                      ) : null}
                      <IssueText issues={rowIssues} />
                      {unsupportedParts.length > 0 ? (
                        <span className="designer__issue designer__issue--warning">
                          Not supported by this database: {unsupportedParts.join(", ")}
                        </span>
                      ) : null}
                    </td>
                  </tr>
                ) : null}
              </Fragment>
            );
          })}
        </tbody>
      </table>
      <div className="designer__add">
        <button
          type="button"
          className="button"
          disabled={disabled || !canAdd}
          title={canAdd ? undefined : supportHint(caps.addIndex, "Adding an index")}
          onClick={() => onChange({ ...design, indexes: [...design.indexes, { def: newIndex() }] })}
        >
          + Index
        </button>
      </div>
    </div>
  );
}

// --- foreign keys -----------------------------------------------------------------------

export function ForeignKeysEditor({
  design,
  onChange,
  caps,
  issues,
  isNew,
  disabled,
}: EditorProps) {
  const canAdd = isNew || allowed(caps.addForeignKey);
  const canChange = (item: DesignItem<ForeignKeyDef>) =>
    isNew ||
    item.origin === undefined ||
    (allowed(caps.addForeignKey) && allowed(caps.dropForeignKey));
  const set = (index: number, def: ForeignKeyDef) =>
    onChange({
      ...design,
      foreignKeys: design.foreignKeys.map((it, i) => (i === index ? { ...it, def } : it)),
    });
  const actions: FkAction[] = caps.fkActions.length > 0 ? caps.fkActions : ["no_action"];
  return (
    <div className="designer__section">
      {design.foreignKeys.length === 0 ? <p className="filter__empty">No foreign keys.</p> : null}
      {design.foreignKeys.map((item, index) => {
        const rowIssues = issuesAt(issues, `foreignKeys[${index}]`);
        const locked = disabled || !canChange(item);
        const def = item.def;
        const key = item.origin === undefined ? `new-${index}` : `o${item.origin}`;
        return (
          <fieldset
            key={key}
            className={`designer__card${issueClass(rowIssues)}`}
            aria-label={`Foreign key ${index + 1}`}
          >
            <div className="designer__card-row">
              <label className="designer__inline">
                Name
                <input
                  type="text"
                  aria-label={`Foreign key ${index + 1} name`}
                  value={def.name ?? ""}
                  placeholder="(generated)"
                  maxLength={caps.maxIdentifierLength ?? undefined}
                  disabled={locked}
                  onChange={(e) => {
                    const { name: _n, ...rest } = def;
                    set(index, e.target.value ? { ...rest, name: e.target.value } : rest);
                  }}
                />
              </label>
              <span className="designer__inline">
                Columns
                <DraftInput
                  label={`Foreign key ${index + 1} columns`}
                  value={def.columns.join(", ")}
                  disabled={locked}
                  placeholder="customer_id"
                  onCommit={(text) => set(index, { ...def, columns: splitNames(text) })}
                />
              </span>
              <button
                type="button"
                className="button button--small"
                aria-label={`Remove foreign key ${index + 1}`}
                disabled={
                  disabled || (!isNew && item.origin !== undefined && !allowed(caps.dropForeignKey))
                }
                onClick={() =>
                  onChange({
                    ...design,
                    foreignKeys: design.foreignKeys.filter((_, i) => i !== index),
                  })
                }
              >
                ×
              </button>
            </div>
            <div className="designer__card-row">
              <span className="designer__inline-label">References</span>
              {def.ref_table.schema !== undefined || design.table.schema !== undefined ? (
                <input
                  type="text"
                  aria-label={`Foreign key ${index + 1} referenced schema`}
                  className="designer__short"
                  value={def.ref_table.schema ?? ""}
                  placeholder="schema"
                  disabled={locked}
                  onChange={(e) => {
                    const { schema: _s, ...rest } = def.ref_table;
                    set(index, {
                      ...def,
                      ref_table: e.target.value ? { ...rest, schema: e.target.value } : rest,
                    });
                  }}
                />
              ) : null}
              <input
                type="text"
                aria-label={`Foreign key ${index + 1} referenced table`}
                value={def.ref_table.name}
                placeholder="table"
                disabled={locked}
                onChange={(e) =>
                  set(index, { ...def, ref_table: { ...def.ref_table, name: e.target.value } })
                }
              />
              <DraftInput
                label={`Foreign key ${index + 1} referenced columns`}
                value={def.ref_columns.join(", ")}
                disabled={locked}
                placeholder="id"
                onCommit={(text) => set(index, { ...def, ref_columns: splitNames(text) })}
              />
            </div>
            <div className="designer__card-row">
              <label className="designer__inline">
                On delete
                <select
                  aria-label={`Foreign key ${index + 1} on delete`}
                  value={def.on_delete}
                  disabled={locked}
                  onChange={(e) => set(index, { ...def, on_delete: e.target.value as FkAction })}
                >
                  {(actions.includes(def.on_delete) ? actions : [def.on_delete, ...actions]).map(
                    (a) => (
                      <option key={a} value={a}>
                        {FK_LABEL[a]}
                      </option>
                    ),
                  )}
                </select>
              </label>
              <label className="designer__inline">
                On update
                <select
                  aria-label={`Foreign key ${index + 1} on update`}
                  value={def.on_update}
                  disabled={locked}
                  onChange={(e) => set(index, { ...def, on_update: e.target.value as FkAction })}
                >
                  {(actions.includes(def.on_update) ? actions : [def.on_update, ...actions]).map(
                    (a) => (
                      <option key={a} value={a}>
                        {FK_LABEL[a]}
                      </option>
                    ),
                  )}
                </select>
              </label>
            </div>
            <IssueText issues={rowIssues} />
          </fieldset>
        );
      })}
      <div className="designer__add">
        <button
          type="button"
          className="button"
          disabled={disabled || !canAdd}
          title={canAdd ? undefined : supportHint(caps.addForeignKey, "Adding a foreign key")}
          onClick={() =>
            onChange({
              ...design,
              foreignKeys: [...design.foreignKeys, { def: newForeignKey(design) }],
            })
          }
        >
          + Foreign key
        </button>
      </div>
    </div>
  );
}

// --- checks -----------------------------------------------------------------------------

export function ChecksEditor({ design, onChange, caps, issues, isNew, disabled }: EditorProps) {
  const canAdd = isNew || allowed(caps.addCheck);
  const canChange = (item: DesignItem<CheckDef>) =>
    isNew || item.origin === undefined || (allowed(caps.addCheck) && allowed(caps.dropCheck));
  const set = (index: number, def: CheckDef) =>
    onChange({
      ...design,
      checks: design.checks.map((it, i) => (i === index ? { ...it, def } : it)),
    });
  return (
    <div className="designer__section">
      {!caps.checksEnforced ? (
        <p className="banner banner--warning">
          This database version does not enforce CHECK constraints.
        </p>
      ) : null}
      {design.checks.length === 0 ? <p className="filter__empty">No checks.</p> : null}
      {design.checks.map((item, index) => {
        const rowIssues = issuesAt(issues, `checks[${index}]`);
        const locked = disabled || !canChange(item);
        const key = item.origin === undefined ? `new-${index}` : `o${item.origin}`;
        return (
          <div key={key} className={`designer__card${issueClass(rowIssues)}`}>
            <div className="designer__card-row">
              <input
                type="text"
                aria-label={`Check ${index + 1} name`}
                className="designer__short"
                value={item.def.name ?? ""}
                placeholder="(generated)"
                maxLength={caps.maxIdentifierLength ?? undefined}
                disabled={locked}
                onChange={(e) => {
                  const { name: _n, ...rest } = item.def;
                  set(index, e.target.value ? { ...rest, name: e.target.value } : rest);
                }}
              />
              <span className="designer__inline-label">CHECK (</span>
              <input
                type="text"
                className="designer__expr designer__grow"
                aria-label={`Check ${index + 1} expression`}
                value={item.def.expression}
                placeholder="qty >= 0"
                disabled={locked}
                spellCheck={false}
                onChange={(e) => set(index, { ...item.def, expression: e.target.value })}
              />
              <span className="designer__inline-label">)</span>
              <button
                type="button"
                className="button button--small"
                aria-label={`Remove check ${index + 1}`}
                disabled={
                  disabled || (!isNew && item.origin !== undefined && !allowed(caps.dropCheck))
                }
                onClick={() =>
                  onChange({ ...design, checks: design.checks.filter((_, i) => i !== index) })
                }
              >
                ×
              </button>
            </div>
            <IssueText issues={rowIssues} />
          </div>
        );
      })}
      <div className="designer__add">
        <button
          type="button"
          className="button"
          disabled={disabled || !canAdd}
          title={canAdd ? undefined : supportHint(caps.addCheck, "Adding a check")}
          onClick={() =>
            onChange({
              ...design,
              checks: [...design.checks, { def: { expression: "", extras: {} } }],
            })
          }
        >
          + Check
        </button>
      </div>
    </div>
  );
}

// --- options ----------------------------------------------------------------------------

export function OptionsEditor({
  design,
  onChange,
  caps,
  issues,
  engine,
  isNew,
  disabled,
}: EditorProps) {
  const traits = designTraits(engine);
  const canRename = isNew || allowed(caps.renameTable);
  const canComment = allowed(caps.tableComment);
  const canPk = isNew || allowed(caps.changePrimaryKey);
  const nameIssues = issuesAt(issues, "table");
  return (
    <div className="designer__section designer__options">
      <div className="field">
        <label htmlFor="designer-table-name">Table name</label>
        <input
          id="designer-table-name"
          type="text"
          value={design.table.name}
          maxLength={caps.maxIdentifierLength ?? undefined}
          disabled={disabled || !canRename}
          title={canRename ? undefined : supportHint(caps.renameTable, "Renaming the table")}
          spellCheck={false}
          onChange={(e) =>
            onChange({ ...design, table: { ...design.table, name: e.target.value } })
          }
        />
        <IssueText issues={nameIssues} />
        <p className="field__hint">
          In{" "}
          {[design.table.catalog, design.table.schema].filter(Boolean).join(" / ") ||
            "the default database"}
        </p>
      </div>
      <div className="field">
        <label htmlFor="designer-table-comment">Comment</label>
        <textarea
          id="designer-table-comment"
          rows={2}
          value={design.comment ?? ""}
          disabled={disabled || !canComment}
          title={supportHint(caps.tableComment, "Table comments")}
          onChange={(e) => {
            const { comment: _c, ...rest } = design;
            onChange(e.target.value ? { ...rest, comment: e.target.value } : rest);
          }}
        />
      </div>
      {design.primaryKey ? (
        <div className="field">
          <label htmlFor="designer-pk-name">Primary key name</label>
          <input
            id="designer-pk-name"
            type="text"
            value={design.primaryKey.name ?? ""}
            placeholder="(generated)"
            disabled={disabled || !canPk}
            onChange={(e) => {
              const pk = design.primaryKey;
              if (!pk) return;
              const { name: _n, ...rest } = pk;
              onChange({
                ...design,
                primaryKey: e.target.value ? { ...rest, name: e.target.value } : rest,
              });
            }}
          />
          <p className="field__hint">Columns: {design.primaryKey.columns.join(", ")}</p>
          {traits.clusteredIndexes ? (
            <label className="checkbox">
              <input
                type="checkbox"
                aria-label="Clustered primary key"
                // Absent means clustered (the server's default for a primary key).
                checked={boolExtra(design.extras, EXTRA.clustered) ?? true}
                disabled={disabled || !canPk}
                onChange={(e) =>
                  onChange({
                    ...design,
                    extras: withExtra(design.extras, EXTRA.clustered, {
                      type: "bool",
                      value: e.target.checked,
                    }),
                  })
                }
              />
              Clustered (the table's rows are stored in primary key order)
            </label>
          ) : null}
        </div>
      ) : null}
      {caps.notes.length > 0 ? (
        <div className="field">
          <span className="designer__inline-label">About this database</span>
          <ul className="designer__notes">
            {caps.notes.map((n) => (
              <li key={n}>{n}</li>
            ))}
          </ul>
        </div>
      ) : null}
    </div>
  );
}
