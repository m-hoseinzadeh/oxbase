import type { ObjectRef } from "../api";

/**
 * A table's display name with its schema first (`sales.orders`); the catalog stands in where an
 * engine has no schema level (MySQL databases). Used for tab titles, headings and messages so a
 * table is never shown without the schema it lives in.
 */
export function qualifiedName(table: ObjectRef): string {
  return [table.schema ?? table.catalog, table.name].filter(Boolean).join(".");
}
