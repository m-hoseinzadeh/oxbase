/**
 * Pure helpers of the table designer: editing a `TableDesign` (docs/m3-contracts.md §6) while
 * keeping every item's `origin` (so the core sees renames, not drop + add), what the engine's
 * `DesignerCapabilities` allow, and typing hints.
 */
import type {
  ColumnDef,
  DataType,
  DesignColumn,
  DesignerCapabilities,
  DesignIssue,
  Engine,
  ForeignKeyDef,
  IndexDef,
  Support,
  TableDesign,
} from "../../api";

/** Common type names offered per engine (the user may type any other). */
export const TYPE_SUGGESTIONS: Partial<Record<Engine, readonly string[]>> = {
  postgres: [
    "integer",
    "bigint",
    "smallint",
    "numeric(12,2)",
    "real",
    "double precision",
    "boolean",
    "text",
    "varchar(255)",
    "char(1)",
    "date",
    "time",
    "timestamp",
    "timestamptz",
    "interval",
    "uuid",
    "json",
    "jsonb",
    "bytea",
  ],
  mysql: [
    "int",
    "bigint",
    "smallint",
    "tinyint(1)",
    "decimal(12,2)",
    "float",
    "double",
    "varchar(255)",
    "char(1)",
    "text",
    "longtext",
    "date",
    "time",
    "datetime",
    "timestamp",
    "json",
    "blob",
    "varbinary(255)",
  ],
  mariadb: [
    "int",
    "bigint",
    "smallint",
    "tinyint(1)",
    "decimal(12,2)",
    "float",
    "double",
    "varchar(255)",
    "char(1)",
    "text",
    "longtext",
    "date",
    "time",
    "datetime",
    "timestamp",
    "json",
    "uuid",
    "blob",
  ],
  sqlite: ["INTEGER", "REAL", "NUMERIC", "TEXT", "BLOB", "BOOLEAN", "DATE", "DATETIME"],
};

function args(text: string): number[] {
  const m = /\(([^)]*)\)/.exec(text);
  if (!m?.[1]) return [];
  return m[1]
    .split(",")
    .map((p) => Number.parseInt(p.trim(), 10))
    .filter((n) => Number.isFinite(n));
}

/**
 * The engine-neutral type of a type name as typed (`varchar(20)`, `bigint unsigned`). The
 * native text is what the engine runs; this only keeps the neutral type roughly in step.
 */
export function guessDataType(native: string, engine?: Engine): DataType {
  const t = native.trim().toLowerCase();
  const [a, b] = args(t);
  const unsigned = /\bunsigned\b/.test(t);
  const base = t
    .replace(/\(.*?\)/g, "")
    .replace(/\bunsigned\b|\bzerofill\b/g, "")
    .trim();
  if (base.endsWith("[]"))
    return { type: "array", element: guessDataType(native.trim().slice(0, -2), engine) };
  if (base === "tinyint" && a === 1) return { type: "bool" };
  if (["bool", "boolean"].includes(base)) return { type: "bool" };
  if (["tinyint", "int1"].includes(base)) return { type: "int", bits: 8, unsigned };
  if (["smallint", "int2", "smallserial"].includes(base))
    return { type: "int", bits: 16, unsigned };
  if (base === "mediumint") return { type: "int", bits: 32, unsigned };
  if (["int", "integer", "int4", "serial"].includes(base)) {
    // SQLite integers are 64-bit whatever they are called.
    return { type: "int", bits: engine === "sqlite" ? 64 : 32, unsigned };
  }
  if (["bigint", "int8", "bigserial"].includes(base)) return { type: "int", bits: 64, unsigned };
  if (["real", "float4"].includes(base)) return { type: "float", bits: 32 };
  if (["float", "double", "double precision", "float8"].includes(base)) {
    return { type: "float", bits: 64 };
  }
  if (["numeric", "decimal", "dec"].includes(base)) {
    return { type: "decimal", precision: a ?? null, scale: b ?? null };
  }
  if (["char", "character", "bpchar", "nchar"].includes(base)) {
    return { type: "char", length: a ?? null };
  }
  if (["varchar", "character varying", "nvarchar", "varchar2"].includes(base)) {
    return { type: "var_char", length: a ?? null };
  }
  if (/^(tiny|medium|long)?text$|^clob$|^citext$/.test(base)) return { type: "text" };
  if (["binary"].includes(base)) return { type: "binary", length: a ?? null };
  if (["varbinary"].includes(base)) return { type: "var_binary", length: a ?? null };
  if (/^(tiny|medium|long)?blob$|^bytea$/.test(base)) return { type: "var_binary", length: null };
  if (base === "date") return { type: "date" };
  if (["time", "time without time zone"].includes(base))
    return { type: "time", precision: a ?? null };
  if (["timestamptz", "timestamp with time zone"].includes(base)) {
    return { type: "timestamp_tz", precision: a ?? null };
  }
  if (["timestamp", "timestamp without time zone", "datetime"].includes(base)) {
    return { type: "timestamp", precision: a ?? null };
  }
  if (base === "interval") return { type: "interval" };
  if (base === "uuid") return { type: "uuid" };
  if (base === "json") return { type: "json", binary: false };
  if (base === "jsonb") return { type: "json", binary: true };
  return { type: "other", name: native.trim() };
}

export function newColumn(name = ""): DesignColumn {
  const column: ColumnDef = {
    name,
    data_type: { type: "text" },
    native_type: "",
    nullable: true,
    auto_increment: false,
    extras: {},
  };
  return { column };
}

export function newIndex(name = ""): IndexDef {
  return { name, columns: [], unique: false, is_constraint: false, extras: {} };
}

export function newForeignKey(design: TableDesign): ForeignKeyDef {
  return {
    columns: [],
    ref_table: {
      kind: "table",
      ...(design.table.catalog ? { catalog: design.table.catalog } : {}),
      ...(design.table.schema ? { schema: design.table.schema } : {}),
      name: "",
    },
    ref_columns: [],
    on_delete: "no_action",
    on_update: "no_action",
    extras: {},
  };
}

function renameIn(list: readonly string[], from: string, to: string): string[] {
  return list.map((c) => (c === from ? to : c));
}

/** Whether a foreign key points at the designed table itself (by its loaded or new name). */
export function isSelfReference(design: TableDesign, fk: ForeignKeyDef): boolean {
  const ref = fk.ref_table;
  const schema = ref.schema ?? design.table.schema;
  if ((schema ?? "") !== (design.table.schema ?? "")) return false;
  return ref.name === design.table.name || ref.name === design.original?.ref.name;
}

/**
 * Renames column `from` to `to` in the primary key, the indexes and the foreign keys
 * (including the referenced columns of a foreign key to this same table). The column itself
 * keeps its origin, so the core generates a rename. Call it once per committed rename, never
 * per keystroke: partial names would capture references of other columns.
 */
export function renameColumnRefs(design: TableDesign, from: string, to: string): TableDesign {
  if (from === to || from === "") return design;
  return {
    ...design,
    ...(design.primaryKey
      ? {
          primaryKey: {
            ...design.primaryKey,
            columns: renameIn(design.primaryKey.columns, from, to),
          },
        }
      : {}),
    indexes: design.indexes.map((item) => ({
      ...item,
      def: {
        ...item.def,
        columns: item.def.columns.map((c) =>
          !c.is_expression && c.name === from ? { ...c, name: to } : c,
        ),
      },
    })),
    foreignKeys: design.foreignKeys.map((item) => ({
      ...item,
      def: {
        ...item.def,
        columns: renameIn(item.def.columns, from, to),
        ...(isSelfReference(design, item.def)
          ? { ref_columns: renameIn(item.def.ref_columns, from, to) }
          : {}),
      },
    })),
  };
}

/** Why column `index` cannot be renamed to `to`, or `null` when it can. */
export function columnNameProblem(design: TableDesign, index: number, to: string): string | null {
  const col = design.columns[index];
  if (!col) return null;
  if (to === "" && col.column.name !== "") return "A column needs a name";
  if (to !== "" && design.columns.some((c, i) => i !== index && c.column.name === to)) {
    return `Another column is already named ${to}`;
  }
  return null;
}

/**
 * Column `index` renamed to `to`, with its primary key, index and foreign key references
 * following it; an error when the name is empty or taken (nothing changes then).
 */
export function renameColumn(
  design: TableDesign,
  index: number,
  to: string,
): { design: TableDesign } | { error: string } {
  const col = design.columns[index];
  if (!col || col.column.name === to) return { design };
  const problem = columnNameProblem(design, index, to);
  if (problem) return { error: problem };
  const renamed: TableDesign = {
    ...design,
    columns: design.columns.map((c, i) =>
      i === index ? { ...c, column: { ...c.column, name: to } } : c,
    ),
  };
  return { design: renameColumnRefs(renamed, col.column.name, to) };
}

/**
 * The design without the references to removed column `name`. Like PostgreSQL's DROP COLUMN,
 * every primary key, index and foreign key that uses the column goes as a whole (a composite
 * key is never silently narrowed to its other columns, which would change its meaning); a
 * foreign key from this table to itself that references the column goes too. Check
 * expressions are not parsed: the preview shows what the database says about them.
 */
export function dropColumnRefs(design: TableDesign, name: string): TableDesign {
  const next: TableDesign = {
    ...design,
    indexes: design.indexes.filter(
      (item) => !item.def.columns.some((c) => !c.is_expression && c.name === name),
    ),
    foreignKeys: design.foreignKeys.filter(
      (item) =>
        !item.def.columns.includes(name) &&
        !(isSelfReference(design, item.def) && item.def.ref_columns.includes(name)),
    ),
  };
  if (design.primaryKey?.columns.includes(name)) delete next.primaryKey;
  return next;
}

/** What removing column `name` also removes (see `dropColumnRefs`), for the user. */
export function dependentsOfColumn(design: TableDesign, name: string): string[] {
  const out: string[] = [];
  if (design.primaryKey?.columns.includes(name)) {
    out.push(`the primary key (${design.primaryKey.columns.join(", ")})`);
  }
  for (const item of design.indexes) {
    if (item.def.columns.some((c) => !c.is_expression && c.name === name)) {
      out.push(`${item.def.unique ? "unique index" : "index"} ${item.def.name || "(unnamed)"}`);
    }
  }
  for (const item of design.foreignKeys) {
    const self = isSelfReference(design, item.def) && item.def.ref_columns.includes(name);
    if (item.def.columns.includes(name) || self) {
      out.push(`foreign key ${item.def.name || `(${item.def.columns.join(", ")})`}`);
    }
  }
  return out;
}

/** Whether an operation is offered (native or by a rebuild). */
export function allowed(support: Support | undefined): boolean {
  return support !== undefined && support !== "unsupported";
}

/** A hint for an operation done by rebuilding the table. */
export function supportHint(support: Support | undefined, what: string): string | undefined {
  if (support === "unsupported") return `${what} is not supported by this database`;
  if (support === "rebuild") return `${what} rebuilds the table (data is copied)`;
  return undefined;
}

/** Read-only capabilities (also used until the real ones are known). */
export const NO_DESIGNER: DesignerCapabilities = {
  transactionalDdl: false,
  createTable: false,
  dropTable: false,
  renameTable: "unsupported",
  tableComment: "unsupported",
  addColumn: "unsupported",
  dropColumn: "unsupported",
  renameColumn: "unsupported",
  alterColumnType: "unsupported",
  alterColumnNullability: "unsupported",
  alterColumnDefault: "unsupported",
  columnComment: "unsupported",
  columnCollation: "unsupported",
  reorderColumns: "unsupported",
  autoIncrement: false,
  identity: false,
  generatedStored: false,
  generatedVirtual: false,
  changePrimaryKey: "unsupported",
  addIndex: "unsupported",
  dropIndex: "unsupported",
  uniqueConstraints: false,
  partialIndexes: false,
  expressionIndexes: false,
  descendingIndexes: false,
  indexPrefixLengths: false,
  indexMethods: [],
  addForeignKey: "unsupported",
  dropForeignKey: "unsupported",
  fkActions: [],
  deferrableConstraints: false,
  addCheck: "unsupported",
  dropCheck: "unsupported",
  checksEnforced: false,
  maxIdentifierLength: null,
  notes: [],
};

/** Issues whose path is `path` or below it (`columns[2]` matches `columns[2].name`). */
export function issuesAt(issues: readonly DesignIssue[], path: string): DesignIssue[] {
  return issues.filter(
    (i) => i.path === path || i.path.startsWith(`${path}.`) || i.path.startsWith(`${path}[`),
  );
}

export type DefaultMode = "none" | "value" | "expression";

export function defaultMode(column: DesignColumn): DefaultMode {
  if (column.defaultLiteral !== undefined) return "value";
  if (column.column.default !== undefined) return "expression";
  return "none";
}

/** The column with its default in `mode` with `text`. */
export function withDefault(column: DesignColumn, mode: DefaultMode, text: string): DesignColumn {
  const { defaultLiteral: _literal, ...rest } = column;
  const { default: _expr, ...def } = column.column;
  if (mode === "none") return { ...rest, column: def };
  if (mode === "value") return { ...rest, column: def, defaultLiteral: text };
  return { ...rest, column: { ...def, default: text } };
}

/** The design with item `index` of `list` moved by `delta`. */
export function moveItem<T>(list: readonly T[], index: number, delta: number): T[] {
  const target = index + delta;
  if (target < 0 || target >= list.length) return [...list];
  const next = [...list];
  const [item] = next.splice(index, 1);
  next.splice(target, 0, item as T);
  return next;
}

/** Splits "a, b" into column names. */
export function splitNames(text: string): string[] {
  return text
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}
