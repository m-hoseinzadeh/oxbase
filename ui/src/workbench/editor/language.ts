/** Monaco grammars per engine (kept apart from the lazily loaded editor). */
import type { Engine } from "../../api";

/** Monaco language id. */
export type SqlLanguage = "pgsql" | "mysql" | "sql";

const LANGUAGE: Partial<Record<Engine, SqlLanguage>> = {
  postgres: "pgsql",
  mysql: "mysql",
  mariadb: "mysql",
  // Monaco's `sql` grammar is the T-SQL one: `[bracket]` identifiers, `N'...'` strings,
  // `@@variables`, `GO`, `TRY ... CATCH` and the T-SQL built-in functions.
  mssql: "sql",
};

/** The Monaco grammar for an engine's SQL. */
export function languageFor(engine: Engine | undefined): SqlLanguage {
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
