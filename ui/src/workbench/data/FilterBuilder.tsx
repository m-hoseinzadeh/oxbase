/**
 * The filter builder: nested AND / OR groups (each can be negated) of conditions `column op
 * value(s)` with inputs typed by the column, raw SQL conditions, and a toggle to type the whole
 * WHERE as raw SQL. Raw SQL is clearly marked: it is sent as typed and checked by the core.
 */
import { CircleAlert, Filter, Minus, Plus, X } from "lucide-react";
import { type ReactNode, useId } from "react";
import type { FilterOp } from "../../api";
import { ICON_SM } from "../../components/icons";
import { Select } from "../../components/Select";
import {
  type ConditionNode,
  emptyGroup,
  type FilterColumn,
  type FilterDraft,
  type FilterNode,
  type FilterProblem,
  type GroupNode,
  newCondition,
  newRaw,
  opsFor,
  replaceNode,
  withOp,
} from "./filterModel";
import { TypedInput } from "./ValueEditor";

export interface FilterBuilderProps {
  draft: FilterDraft;
  columns: readonly FilterColumn[];
  problems: readonly FilterProblem[];
  onChange: (draft: FilterDraft) => void;
  disabled?: boolean | undefined;
}

interface NodeProps {
  /** The connective shown before the item (WHERE / AND / OR). */
  conj?: ReactNode;
  columns: readonly FilterColumn[];
  problems: ReadonlyMap<string, string>;
  update: (id: string, next: FilterNode | null) => void;
  disabled?: boolean | undefined;
  depth: number;
}

function ConditionRow({
  node,
  conj,
  columns,
  problems,
  update,
  disabled,
}: NodeProps & { node: ConditionNode }) {
  const column = columns.find((c) => c.name === node.column);
  const kind = column?.input ?? { type: "text" as const };
  const ops = opsFor(column?.input);
  const problem = problems.get(node.id);
  const set = (patch: Partial<ConditionNode>) => update(node.id, { ...node, ...patch });
  const many = node.op === "in" || node.op === "notIn";
  return (
    <li className={`filter__item filter__condition${problem ? " filter__item--invalid" : ""}`}>
      {conj}
      <Select
        aria-label="Column"
        className="filter__column"
        value={node.column}
        disabled={disabled}
        onChange={(e) => {
          const next = columns.find((c) => c.name === e.target.value);
          const allowed = opsFor(next?.input).map((o) => o.op);
          const op = allowed.includes(node.op) ? node.op : "eq";
          update(node.id, withOp({ ...node, column: e.target.value }, op));
        }}
      >
        {node.column === "" ? <option value="">Column…</option> : null}
        {node.column !== "" && !column ? <option value={node.column}>{node.column}</option> : null}
        {columns.map((c) => (
          <option key={c.name} value={c.name}>
            {c.name}
          </option>
        ))}
      </Select>
      <Select
        aria-label="Operator"
        className="filter__op"
        value={node.op}
        disabled={disabled}
        onChange={(e) => update(node.id, withOp(node, e.target.value as FilterOp))}
      >
        {ops.map((o) => (
          <option key={o.op} value={o.op}>
            {o.label}
          </option>
        ))}
      </Select>
      {node.values.map((value, index) => (
        // Values have no identity of their own; their position is their key.
        // biome-ignore lint/suspicious/noArrayIndexKey: positional inputs.
        <span key={index} className="filter__value">
          {index > 0 && !many ? <span className="filter__and">and</span> : null}
          <TypedInput
            kind={kind}
            compact
            text={value}
            label={node.values.length > 1 ? `Value ${index + 1}` : "Value"}
            disabled={disabled}
            onChange={(text) =>
              set({ values: node.values.map((v, i) => (i === index ? text : v)) })
            }
          />
          {many && node.values.length > 1 ? (
            <button
              type="button"
              className="icon-button icon-button--small"
              aria-label={`Remove value ${index + 1}`}
              title={`Remove value ${index + 1}`}
              disabled={disabled}
              onClick={() => set({ values: node.values.filter((_, i) => i !== index) })}
            >
              <Minus {...ICON_SM} />
            </button>
          ) : null}
        </span>
      ))}
      {many ? (
        <button
          type="button"
          className="button button--ghost button--small"
          disabled={disabled}
          onClick={() => set({ values: [...node.values, ""] })}
        >
          <Plus {...ICON_SM} />
          Add value
        </button>
      ) : null}
      <button
        type="button"
        className="icon-button icon-button--small filter__remove"
        aria-label="Remove condition"
        title="Remove condition"
        disabled={disabled}
        onClick={() => update(node.id, null)}
      >
        <X {...ICON_SM} />
      </button>
      {problem ? (
        <span className="filter__problem">
          <CircleAlert {...ICON_SM} />
          {problem}
        </span>
      ) : null}
    </li>
  );
}

function RawRow({
  node,
  conj,
  problems,
  update,
  disabled,
}: NodeProps & { node: FilterNode & { kind: "raw" } }) {
  const problem = problems.get(node.id);
  return (
    <li className={`filter__item filter__raw${problem ? " filter__item--invalid" : ""}`}>
      {conj}
      <span className="raw-badge" title="Sent as typed; checked to be a single expression">
        RAW SQL
      </span>
      <input
        type="text"
        aria-label="Raw SQL condition"
        className="filter__raw-input"
        spellCheck={false}
        placeholder="price * qty > 100"
        value={node.sql}
        disabled={disabled}
        onChange={(e) => update(node.id, { ...node, sql: e.target.value })}
      />
      <button
        type="button"
        className="icon-button icon-button--small filter__remove"
        aria-label="Remove raw condition"
        title="Remove raw condition"
        disabled={disabled}
        onClick={() => update(node.id, null)}
      >
        <X {...ICON_SM} />
      </button>
      {problem ? (
        <span className="filter__problem">
          <CircleAlert {...ICON_SM} />
          {problem}
        </span>
      ) : null}
    </li>
  );
}

function GroupBox({
  node,
  columns,
  problems,
  update,
  disabled,
  depth,
}: NodeProps & { node: GroupNode }) {
  const set = (patch: Partial<GroupNode>) => update(node.id, { ...node, ...patch });
  const firstColumn = columns[0]?.name ?? "";
  const joinWord = node.join === "and" ? "AND" : "OR";
  return (
    <fieldset
      className={`filter__group${depth === 0 ? " filter__group--root" : ""}${node.negate ? " filter__group--negated" : ""}`}
      aria-label={depth === 0 ? "Filter" : "Group"}
    >
      <div className="filter__group-head">
        <label className="filter__not" title="Negate the whole group">
          <input
            type="checkbox"
            checked={node.negate}
            disabled={disabled}
            onChange={(e) => set({ negate: e.target.checked })}
          />
          NOT
        </label>
        <fieldset className="filter__join" aria-label="Match">
          {(["and", "or"] as const).map((join) => (
            <button
              key={join}
              type="button"
              className="filter__join-option"
              aria-pressed={node.join === join}
              disabled={disabled}
              title={join === "and" ? "Rows match all conditions" : "Rows match any condition"}
              onClick={() => set({ join })}
            >
              {join === "and" ? "AND" : "OR"}
            </button>
          ))}
        </fieldset>
        <span className="filter__join-hint">{node.join === "and" ? "all of" : "any of"}</span>
        <span className="filter__group-actions">
          <button
            type="button"
            className="button button--ghost button--small"
            disabled={disabled}
            title="Add a condition"
            onClick={() => set({ items: [...node.items, newCondition(firstColumn)] })}
          >
            <Plus {...ICON_SM} />
            Condition
          </button>
          <button
            type="button"
            className="button button--ghost button--small"
            disabled={disabled || depth >= 30}
            title="Add a nested group"
            onClick={() =>
              set({ items: [...node.items, emptyGroup(node.join === "and" ? "or" : "and")] })
            }
          >
            <Plus {...ICON_SM} />
            Group
          </button>
          <button
            type="button"
            className="button button--ghost button--small"
            disabled={disabled}
            title="Add a raw SQL condition"
            onClick={() => set({ items: [...node.items, newRaw()] })}
          >
            <Plus {...ICON_SM} />
            Raw SQL
          </button>
          {depth > 0 ? (
            <button
              type="button"
              className="icon-button icon-button--small filter__remove"
              aria-label="Remove group"
              title="Remove group"
              disabled={disabled}
              onClick={() => update(node.id, null)}
            >
              <X {...ICON_SM} />
            </button>
          ) : null}
        </span>
      </div>
      {node.items.length === 0 ? (
        <p className="filter__empty">
          {depth === 0 ? "No conditions: every row is shown." : "Empty group (ignored)."}
        </p>
      ) : (
        <ul className="filter__items">
          {node.items.map((item, i) => {
            const props = { columns, problems, update, disabled, depth: depth + 1 };
            const conj = (
              <span className="filter__conj" aria-hidden="true">
                {i === 0 ? (depth === 0 ? "WHERE" : "") : joinWord}
              </span>
            );
            if (item.kind === "condition")
              return <ConditionRow key={item.id} node={item} conj={conj} {...props} />;
            if (item.kind === "raw")
              return <RawRow key={item.id} node={item} conj={conj} {...props} />;
            return (
              <li key={item.id} className="filter__item filter__item--group">
                {conj}
                <GroupBox node={item} {...props} />
              </li>
            );
          })}
        </ul>
      )}
    </fieldset>
  );
}

export function FilterBuilder({
  draft,
  columns,
  problems,
  onChange,
  disabled,
}: FilterBuilderProps) {
  const modeName = useId();
  const problemMap = new Map(problems.map((p) => [p.id, p.message]));
  const update = (id: string, next: FilterNode | null) =>
    onChange({ ...draft, root: replaceNode(draft.root, id, next) });
  return (
    <div className="filter" data-testid="filter-builder">
      <div className="builder__head">
        <span className="builder__title">
          <Filter {...ICON_SM} />
          Filter
        </span>
        <div className="filter__mode" role="radiogroup" aria-label="Filter mode">
          <label className="filter__mode-option">
            <input
              type="radio"
              name={modeName}
              checked={draft.mode === "builder"}
              disabled={disabled}
              onChange={() => onChange({ ...draft, mode: "builder" })}
            />
            Builder
          </label>
          <label className="filter__mode-option">
            <input
              type="radio"
              name={modeName}
              checked={draft.mode === "raw"}
              disabled={disabled}
              onChange={() => onChange({ ...draft, mode: "raw" })}
            />
            Raw WHERE
          </label>
        </div>
      </div>
      {draft.mode === "raw" ? (
        <div className="filter__raw-where">
          <span className="raw-badge">RAW SQL</span>
          <span className="filter__where">WHERE</span>
          <textarea
            aria-label="Raw WHERE clause"
            className="filter__raw-text"
            spellCheck={false}
            rows={2}
            placeholder="status = 'open' AND created_at > CURRENT_DATE - 7"
            value={draft.raw}
            disabled={disabled}
            onChange={(e) => onChange({ ...draft, raw: e.target.value })}
          />
          <p className="field__hint">
            Sent to the database as typed. It must be one condition: no semicolons or comments.
          </p>
        </div>
      ) : (
        <GroupBox
          node={draft.root}
          columns={columns}
          problems={problemMap}
          update={update}
          disabled={disabled}
          depth={0}
        />
      )}
    </div>
  );
}
