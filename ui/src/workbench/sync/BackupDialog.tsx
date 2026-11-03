/**
 * Backs up a database or schema into an Oxbase archive (`.oxbackup`, docs/m8-contracts.md §7):
 * pick the container, optionally some of its tables, the content and the compression, then a
 * file in the save dialog. Progress per object; the report gives the size and what went in.
 * The archive never holds connection details.
 */
import { Archive, CircleX, LoaderCircle } from "lucide-react";
import { type ReactNode, useEffect, useId, useReducer, useRef, useState } from "react";
import {
  BACKUP_FILTERS,
  type BackupContent,
  backupCancel,
  backupRun,
  filePickSave,
  type ObjectRef,
} from "../../api";
import { ICON, ICON_SM } from "../../components/icons";
import { Modal } from "../../components/Modal";
import { newRequestId, toCommandError } from "../../connections/api";
import { useConnections } from "../../connections/store";
import { formatMs } from "../format";
import { engineFamily } from "../nosql/engines";
import { formatBytes } from "../transfer/model";
import { ContainerPicker, type SideValue, sideFromInit } from "./ContainerPicker";
import { listTables } from "./catalog";
import {
  backupFileName,
  backupReducer,
  containerLabel,
  ended,
  IDLE_BACKUP,
  plural,
  qualifiedName,
} from "./model";
import { Note, RunBar, WarningList } from "./parts";
import { type BackupInit, setSyncBusy } from "./store";

type Listing =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "ok"; refs: ObjectRef[] }
  | { status: "error"; message: string };

export function BackupDialog({ init, onClose }: { init: BackupInit; onClose: () => void }) {
  const { sessions } = useConnections();
  const id = useId();
  const sqlSessions = sessions.filter((s) => engineFamily(s.engine) === "sql");
  const [side, setSide] = useState<SideValue>(() =>
    sideFromInit(init.source, sqlSessions[0]?.id ?? ""),
  );
  const [scope, setScope] = useState<"all" | "some">("all");
  const [listing, setListing] = useState<Listing>({ status: "idle" });
  const [picked, setPicked] = useState<string[]>([]);
  const [content, setContent] = useState<BackupContent>("structure_and_data");
  const [zstd, setZstd] = useState(true);
  const [pickError, setPickError] = useState<string | null>(null);
  const [fileName, setFileName] = useState<string | null>(null);
  const [run, dispatch] = useReducer(backupReducer, IDLE_BACKUP);
  const alive = useRef(true);
  const runId = useRef<string | null>(null);
  const running = run.status === "starting" || run.status === "running";
  const containerKey = side.container ? JSON.stringify(side.container) : "";

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      if (runId.current) void backupCancel(runId.current).catch(() => {});
    };
  }, []);

  useEffect(() => {
    setSyncBusy(running);
    return () => setSyncBusy(false);
  }, [running]);

  useEffect(() => {
    if (!containerKey || scope !== "some") return;
    let current = true;
    setListing({ status: "loading" });
    listTables(side.sessionId, JSON.parse(containerKey) as ObjectRef, true, newRequestId()).then(
      (refs) => current && setListing({ status: "ok", refs }),
      (error) => current && setListing({ status: "error", message: toCommandError(error).message }),
    );
    return () => {
      current = false;
    };
  }, [side.sessionId, containerKey, scope]);

  const refs = listing.status === "ok" ? listing.refs : [];
  const objects = scope === "some" ? refs.filter((r) => picked.includes(qualifiedName(r))) : [];
  const canStart = !running && side.container !== null && (scope === "all" || objects.length > 0);

  async function start() {
    if (!canStart || !side.container) return;
    setPickError(null);
    let file: Awaited<ReturnType<typeof filePickSave>>;
    try {
      file = await filePickSave(backupFileName(side.container), BACKUP_FILTERS, "Save the backup");
    } catch (error) {
      setPickError(toCommandError(error).message);
      return;
    }
    if (!file || !alive.current) return;
    setFileName(file.name);
    dispatch({ type: "start" });
    try {
      const opId = await backupRun(
        file.handle,
        {
          session: side.sessionId,
          spec: {
            container: side.container,
            ...(objects.length > 0 ? { objects } : {}),
            content,
            compression: { codec: zstd ? "zstd" : "none" },
          },
        },
        (event) => {
          if (!alive.current) return;
          dispatch({ type: "event", event });
          if (ended(event.type)) runId.current = null;
        },
      );
      runId.current = opId;
      if (alive.current) dispatch({ type: "started", id: opId });
    } catch (error) {
      if (alive.current) dispatch({ type: "startFailed", error: toCommandError(error) });
    }
  }

  function cancel() {
    const opId = runId.current ?? run.id;
    if (!opId) return;
    dispatch({ type: "cancelling" });
    void backupCancel(opId).catch(() => {});
  }

  function close() {
    if (!running) onClose();
  }

  function form(): ReactNode {
    return (
      <>
        <ContainerPicker
          label="Back up"
          sessions={sqlSessions}
          value={side}
          testId="backup-source"
          emptyHint="Open a connection to a SQL database first."
          onChange={setSide}
        />
        <fieldset className="fieldset">
          <legend className="transfer-label">Objects</legend>
          <div className="segmented" role="radiogroup" aria-label="Objects">
            {(["all", "some"] as const).map((v) => (
              <label key={v} className="segmented__item transfer-radio">
                <input
                  type="radio"
                  name={`${id}-scope`}
                  checked={scope === v}
                  onChange={() => setScope(v)}
                />
                {v === "all" ? "Everything in it" : "Chosen tables and views"}
              </label>
            ))}
          </div>
        </fieldset>
        {scope === "some" ? (
          listing.status === "loading" ? (
            <p className="transfer-step__status" role="status">
              <LoaderCircle {...ICON_SM} className="spin" />
              Listing…
            </p>
          ) : listing.status === "error" ? (
            <Note tone="error">{listing.message}</Note>
          ) : (
            <ul className="sync-checklist sync-checklist--short" aria-label="Objects to back up">
              {refs.map((r) => {
                const key = qualifiedName(r);
                return (
                  <li key={`${r.kind}:${key}`}>
                    <label className="checkbox">
                      <input
                        type="checkbox"
                        checked={picked.includes(key)}
                        onChange={(e) =>
                          setPicked(
                            e.target.checked ? [...picked, key] : picked.filter((k) => k !== key),
                          )
                        }
                      />
                      {key}
                      {r.kind === "view" ? <span className="badge badge--info">view</span> : null}
                    </label>
                  </li>
                );
              })}
            </ul>
          )
        ) : null}
        <div className="field-row">
          <div className="field">
            <label htmlFor={`${id}-content`}>Content</label>
            <select
              id={`${id}-content`}
              value={content}
              onChange={(e) => setContent(e.target.value as BackupContent)}
            >
              <option value="structure_and_data">Structure and data</option>
              <option value="structure_only">Structure only</option>
            </select>
          </div>
          <label className="checkbox sync-inline-check">
            <input type="checkbox" checked={zstd} onChange={(e) => setZstd(e.target.checked)} />
            Compress table data (zstd)
          </label>
        </div>
        <p className="field__hint">
          The archive holds object definitions, their DDL and the table rows, each with a checksum.
          It never holds hosts, users or passwords.
        </p>
        {pickError ? <Note tone="error">{pickError}</Note> : null}
      </>
    );
  }

  function progress(): ReactNode {
    const p = run.progress;
    const finished = ended(run.status);
    return (
      <div className="sync-run" data-testid="backup-progress">
        <p className="transfer-step__status" role="status" data-testid="backup-status">
          {running ? <LoaderCircle {...ICON_SM} className="spin" /> : null}
          {run.status === "starting"
            ? "Starting…"
            : run.status === "running"
              ? `Backing up ${containerLabel(side.container)} into ${fileName ?? "the file"}${p?.current ? ` · ${qualifiedName(p.current)}` : ""}`
              : run.status === "finished"
                ? "Backup finished"
                : run.status === "failed"
                  ? "Backup failed"
                  : run.status === "cancelled"
                    ? "Backup cancelled"
                    : ""}
        </p>
        {!finished ? (
          <RunBar
            done={p?.objectsDone ?? 0}
            total={p?.objectsTotal ?? (run.objects || null)}
            label="Backup progress"
          />
        ) : null}
        {p ? (
          <dl className="transfer-summary">
            <dt>Objects</dt>
            <dd className="transfer-summary__num">{`${p.objectsDone} / ${p.objectsTotal}`}</dd>
            <dt>Rows</dt>
            <dd className="transfer-summary__num">{p.rows.toLocaleString("en-US")}</dd>
            <dt>Written</dt>
            <dd className="transfer-summary__num">{formatBytes(p.bytes)}</dd>
            <dt>Time</dt>
            <dd className="transfer-summary__num">{formatMs(run.elapsedMs)}</dd>
          </dl>
        ) : null}
        {run.status === "finished" && run.report ? (
          <>
            <Note tone="ok" title="Backup written" testId="backup-result" role="status">
              {`${fileName ?? "The archive"}: ${plural(run.report.manifest.objects.length, "object", "objects")}, ${formatBytes(run.report.bytes)}, in ${formatMs(run.report.elapsedMs)}.`}
            </Note>
            <WarningList title="Warnings" items={run.report.manifest.warnings} />
          </>
        ) : null}
        {run.status === "failed" ? (
          <Note tone="error" title="The backup failed" testId="backup-result">
            {`${run.error?.message ?? ""} No archive was left behind.`}
          </Note>
        ) : null}
        {run.status === "cancelled" ? (
          <Note tone="warning" title="Backup cancelled" testId="backup-result">
            No archive was left behind.
          </Note>
        ) : null}
      </div>
    );
  }

  const footer =
    run.status === "idle" ? (
      <>
        <button type="button" className="button modal__footer-start" onClick={close}>
          Cancel
        </button>
        <button
          type="button"
          className="button button--primary"
          data-testid="backup-start"
          disabled={!canStart}
          onClick={() => void start()}
        >
          <Archive {...ICON} />
          Back up…
        </button>
      </>
    ) : running ? (
      <button
        type="button"
        className="button button--danger"
        data-testid="backup-cancel"
        disabled={run.cancelling || run.status === "starting"}
        onClick={cancel}
      >
        <CircleX {...ICON} />
        {run.cancelling ? "Cancelling…" : "Cancel backup"}
      </button>
    ) : (
      <button type="button" className="button button--primary" onClick={onClose}>
        Close
      </button>
    );

  return (
    <Modal title="Back up" onClose={close} className="modal--transfer-narrow" footer={footer}>
      {run.status === "idle" ? form() : progress()}
    </Modal>
  );
}
