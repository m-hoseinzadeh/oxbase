/** Monaco grammars per engine (kept apart from the lazily loaded editor). */
import type { Engine } from "../../api";

/** Monaco language id of a SQL dialect. */
export type SqlLanguage = "pgsql" | "mysql" | "sql";

/**
 * Monaco language id of any editor: SQL, the mongo shell (`javascript`, also used for the
 * Extended JSON editors of the document builder and JSON values), the Redis console (`redis`),
 * and the value panel's `xml` and `plaintext`.
 */
export type EditorLanguage = SqlLanguage | "javascript" | "redis" | "xml" | "plaintext";

const LANGUAGE: Partial<Record<Engine, EditorLanguage>> = {
  postgres: "pgsql",
  mysql: "mysql",
  mariadb: "mysql",
  // Monaco's `sql` grammar is the T-SQL one: `[bracket]` identifiers, `N'...'` strings,
  // `@@variables`, `GO`, `TRY ... CATCH` and the T-SQL built-in functions.
  mssql: "sql",
  // M7: the mongo-shell subset is JavaScript; the Redis console is one command per line.
  mongodb: "javascript",
  redis: "redis",
};

/** The Monaco grammar for an engine's query language. */
export function languageFor(engine: Engine | undefined): EditorLanguage {
  return (engine && LANGUAGE[engine]) ?? "sql";
}

/**
 * A note shown in the Messages tab for engines whose driver cannot report everything the server
 * sends (docs/m4-contracts.md §11): the user should not read "no messages" as "none were sent".
 */
const MESSAGES_NOTE: Partial<Record<Engine, string>> = {
  mssql:
    "SQL Server: PRINT output and informational messages (RAISERROR with severity 10 or lower) are not shown yet, and plain batches report no row counts.",
};

export function messagesNoteFor(engine: Engine | undefined): string | null {
  return (engine && MESSAGES_NOTE[engine]) ?? null;
}
