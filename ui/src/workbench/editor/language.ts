/** Monaco grammars per engine (kept apart from the lazily loaded editor). */
import type { Engine } from "../../api";

/** Monaco language id. */
export type SqlLanguage = "pgsql" | "mysql" | "sql";

const LANGUAGE: Partial<Record<Engine, SqlLanguage>> = {
  postgres: "pgsql",
  mysql: "mysql",
  mariadb: "mysql",
};

/** The Monaco grammar for an engine's SQL. */
export function languageFor(engine: Engine | undefined): SqlLanguage {
  return (engine && LANGUAGE[engine]) ?? "sql";
}
