/**
 * Picks a connection and a database or schema of it: the source or target of a transfer, a
 * sync, a backup or a restore. Databases and schemas are listed through the object tree; when a
 * database has schemas, a schema must be chosen, else the database is the container.
 */
import { CircleAlert, LoaderCircle, RotateCw } from "lucide-react";
import { useEffect, useId, useState } from "react";
import type { ObjectRef } from "../../api";
import { ICON_SM } from "../../components/icons";
import { newRequestId, toCommandError } from "../../connections/api";
import { ENGINE_INFO, type SessionSummary } from "../../connections/types";
import { listDatabases, listSchemas } from "./catalog";
import { sameRef } from "./model";
import type { SideInit } from "./store";

/** What a picker holds: names of the chosen database and schema, and the container they make. */
export interface SideValue {
  sessionId: string;
  database: string;
  schema: string;
  /** The database or schema ref, once the choice is complete. */
  container: ObjectRef | null;
}

export const EMPTY_SIDE: SideValue = { sessionId: "", database: "", schema: "", container: null };

/** A picker value from an entry point's hint (a connection and maybe a database or schema). */
export function sideFromInit(init: SideInit | undefined, fallbackSession = ""): SideValue {
  if (!init) return { ...EMPTY_SIDE, sessionId: fallbackSession };
  const c = init.container;
  if (c?.kind === "schema") {
    return {
      sessionId: init.sessionId,
      database: c.catalog ?? "",
      schema: c.name,
      container: null,
    };
  }
  if (c?.kind === "database") {
    return { sessionId: init.sessionId, database: c.name, schema: "", container: null };
  }
  return { ...EMPTY_SIDE, sessionId: init.sessionId };
}

type Listing =
  | { status: "loading" }
  | { status: "ok"; refs: ObjectRef[] }
  | { status: "error"; message: string };

export interface ContainerPickerProps {
  /** `Source` or `Target`: prefixes the field labels. */
  label: string;
  sessions: SessionSummary[];
  value: SideValue;
  onChange: (value: SideValue) => void;
  disabled?: boolean | undefined;
  /** Why a session is not offered (shown when none is). */
  emptyHint?: string | undefined;
  testId?: string | undefined;
}

export function ContainerPicker({
  label,
  sessions,
  value,
  onChange,
  disabled,
  emptyHint,
  testId,
}: ContainerPickerProps) {
  const id = useId();
  const [databases, setDatabases] = useState<Listing | null>(null);
  const [schemas, setSchemas] = useState<Listing | null>(null);
  const [reload, setReload] = useState(0);
  const session = sessions.find((s) => s.id === value.sessionId) ?? null;
  const sessionId = session?.id ?? "";

  // Databases of the chosen connection.
  useEffect(() => {
    void reload;
    if (!sessionId) {
      setDatabases(null);
      return;
    }
    let current = true;
    setDatabases({ status: "loading" });
    listDatabases(sessionId, newRequestId()).then(
      (refs) => current && setDatabases({ status: "ok", refs }),
      (error) =>
        current && setDatabases({ status: "error", message: toCommandError(error).message }),
    );
    return () => {
      current = false;
    };
  }, [sessionId, reload]);

  const databaseRef =
    databases?.status === "ok"
      ? (databases.refs.find((d) => d.name === value.database) ?? null)
      : null;

  // Schemas of the chosen database.
  const databaseKey = databaseRef ? JSON.stringify(databaseRef) : "";
  useEffect(() => {
    void reload;
    if (!sessionId || !databaseKey) {
      setSchemas(null);
      return;
    }
    let current = true;
    setSchemas({ status: "loading" });
    listSchemas(sessionId, JSON.parse(databaseKey) as ObjectRef, newRequestId()).then(
      (refs) => current && setSchemas({ status: "ok", refs }),
      (error) => current && setSchemas({ status: "error", message: toCommandError(error).message }),
    );
    return () => {
      current = false;
    };
  }, [sessionId, databaseKey, reload]);

  // A default database once the list is in: the hinted one, the connection's own, or the only one.
  useEffect(() => {
    if (databases?.status !== "ok" || databaseRef) return;
    const refs = databases.refs;
    const pick =
      refs.find((d) => d.name === value.database) ??
      (value.database ? undefined : refs.find((d) => d.name === session?.database)) ??
      (refs.length === 1 ? refs[0] : undefined);
    if (pick && pick.name !== value.database) {
      onChange({ ...value, database: pick.name, schema: value.schema, container: null });
    }
  });

  // The container the choice makes; reported when it changes.
  let container: ObjectRef | null = null;
  if (databaseRef && schemas?.status === "ok") {
    if (schemas.refs.length === 0) container = databaseRef;
    else container = schemas.refs.find((s) => s.name === value.schema) ?? null;
  }
  useEffect(() => {
    const changed =
      container === null ? value.container !== null : !sameRef(container, value.container);
    if (changed) onChange({ ...value, container });
  });

  const hasSchemas = schemas?.status === "ok" && schemas.refs.length > 0;
  const problem =
    databases?.status === "error"
      ? databases.message
      : schemas?.status === "error"
        ? schemas.message
        : null;

  return (
    <div className="sync-side" data-testid={testId}>
      <div className="field">
        <label htmlFor={`${id}-session`}>{label} connection</label>
        <select
          id={`${id}-session`}
          value={sessionId}
          disabled={disabled}
          onChange={(e) =>
            onChange({ sessionId: e.target.value, database: "", schema: "", container: null })
          }
        >
          {session ? null : <option value="">Choose a connection</option>}
          {sessions.map((s) => (
            <option key={s.id} value={s.id}>
              {s.profile_name} ({ENGINE_INFO[s.engine].label})
            </option>
          ))}
        </select>
        {sessions.length === 0 && emptyHint ? <p className="field__hint">{emptyHint}</p> : null}
      </div>
      <div className="field">
        <label htmlFor={`${id}-database`}>{label} database</label>
        <select
          id={`${id}-database`}
          value={databaseRef ? value.database : ""}
          disabled={disabled || databases?.status !== "ok"}
          onChange={(e) =>
            onChange({ ...value, database: e.target.value, schema: "", container: null })
          }
        >
          {databaseRef ? null : (
            <option value="">
              {databases?.status === "loading" ? "Loading…" : "Choose a database"}
            </option>
          )}
          {databases?.status === "ok"
            ? databases.refs.map((d) => (
                <option key={d.name} value={d.name}>
                  {d.name}
                </option>
              ))
            : null}
        </select>
      </div>
      {hasSchemas || (databaseRef && schemas?.status === "loading") ? (
        <div className="field">
          <label htmlFor={`${id}-schema`}>{label} schema</label>
          <select
            id={`${id}-schema`}
            value={container ? value.schema : ""}
            disabled={disabled || schemas?.status !== "ok"}
            onChange={(e) => onChange({ ...value, schema: e.target.value, container: null })}
          >
            {container ? null : (
              <option value="">
                {schemas?.status === "loading" ? "Loading…" : "Choose a schema"}
              </option>
            )}
            {schemas?.status === "ok"
              ? schemas.refs.map((s) => (
                  <option key={s.name} value={s.name}>
                    {s.name}
                  </option>
                ))
              : null}
          </select>
        </div>
      ) : null}
      {databases?.status === "loading" || schemas?.status === "loading" ? (
        <span className="sync-side__state" role="status">
          <LoaderCircle {...ICON_SM} className="spin" />
          Listing…
        </span>
      ) : null}
      {problem ? (
        <p className="field__error sync-side__error" role="alert">
          <CircleAlert {...ICON_SM} />
          {problem}
          <button
            type="button"
            className="button button--small button--ghost"
            onClick={() => setReload((n) => n + 1)}
          >
            <RotateCw {...ICON_SM} />
            Retry
          </button>
        </p>
      ) : null}
    </div>
  );
}
