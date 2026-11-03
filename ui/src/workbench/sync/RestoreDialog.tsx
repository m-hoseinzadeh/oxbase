/**
 * Restores an Oxbase archive into an existing database or schema of the same engine, or only
 * verifies it (docs/m8-contracts.md §7.3-7.4): open the file, read its manifest, check every
 * checksum, choose the target, the content and the objects; dropping existing objects and
 * emptying tables before the load are destructive and need confirmation.
 */
import {
  ArchiveRestore,
  CircleCheck,
  CircleX,
  FolderOpen,
  LoaderCircle,
  ShieldCheck,
} from "lucide-react";
import { type ReactNode, useEffect, useId, useReducer, useRef, useState } from "react";
import {
  BACKUP_FILTERS,
  backupCancel,
  backupList,
  backupRestore,
  backupVerify,
  filePickOpen,
  fileRelease,
  type Manifest,
  type PickedFile,
  type RestoreContent,
  type VerifyReport,
} from "../../api";
import { ICON, ICON_SM } from "../../components/icons";
import { Modal } from "../../components/Modal";
import { cancelRequest, newRequestId, toCommandError } from "../../connections/api";
import { useConnections } from "../../connections/store";
import { ENGINE_INFO } from "../../connections/types";
import { formatDateTime, formatMs } from "../format";
import { formatBytes } from "../transfer/model";
import { ContainerPicker, type SideValue, sideFromInit } from "./ContainerPicker";
import {
  containerLabel,
  ended,
  IDLE_RESTORE,
  plural,
  qualifiedName,
  restoreReducer,
} from "./model";
import { Acknowledge, Note, RunBar, WarningList } from "./parts";
import { type RestoreInit, setSyncBusy } from "./store";

type ManifestState =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "ok"; manifest: Manifest }
  | { status: "error"; message: string };

type VerifyState =
  | { status: "idle" }
  | { status: "running"; requestId: string }
  | { status: "ok"; report: VerifyReport }
  | { status: "error"; message: string };

const RESTORED_TONE = {
  created: "success",
  replaced: "warning",
  kept: "info",
  loaded: "success",
  failed: "danger",
  not_run: "info",
} as const;

export function RestoreDialog({ init, onClose }: { init: RestoreInit; onClose: () => void }) {
  const { sessions } = useConnections();
  const id = useId();
  const verifyOnly = init.verifyOnly ?? false;
  const [file, setFile] = useState<PickedFile | null>(null);
  const [picking, setPicking] = useState(false);
  const [pickError, setPickError] = useState<string | null>(null);
  const [manifest, setManifest] = useState<ManifestState>({ status: "idle" });
  const [verify, setVerify] = useState<VerifyState>({ status: "idle" });
  const [target, setTarget] = useState<SideValue>(() => sideFromInit(init.target));
  const [content, setContent] = useState<RestoreContent>("structure_and_data");
  const [excluded, setExcluded] = useState<number[]>([]);
  const [dropExisting, setDropExisting] = useState(false);
  const [truncate, setTruncate] = useState(false);
  const [verifyFirst, setVerifyFirst] = useState(true);
  const [acknowledged, setAcknowledged] = useState(false);
  const [run, dispatch] = useReducer(restoreReducer, IDLE_RESTORE);
  const alive = useRef(true);
  const fileRef = useRef<PickedFile | null>(null);
  const runId = useRef<string | null>(null);
  const requests = useRef<string[]>([]);
  fileRef.current = file;
  const running = run.status === "starting" || run.status === "running";

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      if (runId.current) void backupCancel(runId.current).catch(() => {});
      for (const r of requests.current) void cancelRequest(r).catch(() => {});
      const f = fileRef.current;
      if (f) void fileRelease(f.handle).catch(() => {});
    };
  }, []);

  useEffect(() => {
    setSyncBusy(running);
    return () => setSyncBusy(false);
  }, [running]);

  const m = manifest.status === "ok" ? manifest.manifest : null;
  const sameEngine = sessions.filter((s) => !m || s.engine === m.engine);
  const destructive = dropExisting || (truncate && content !== "structure_only");
  const chosen = m ? m.objects.filter((o) => !excluded.includes(o.index)).map((o) => o.index) : [];
  const canRestore =
    !running &&
    m !== null &&
    file !== null &&
    target.container !== null &&
    chosen.length > 0 &&
    (!destructive || acknowledged);

  async function pick() {
    setPickError(null);
    setPicking(true);
    try {
      const picked = await filePickOpen(BACKUP_FILTERS, "Open a backup");
      if (!picked || !alive.current) return;
      const old = fileRef.current;
      if (old) void fileRelease(old.handle).catch(() => {});
      setFile(picked);
      setVerify({ status: "idle" });
      setExcluded([]);
      setManifest({ status: "loading" });
      const requestId = newRequestId();
      requests.current.push(requestId);
      try {
        const next = await backupList(picked.handle, requestId);
        if (alive.current) setManifest({ status: "ok", manifest: next });
      } catch (error) {
        if (alive.current) setManifest({ status: "error", message: toCommandError(error).message });
      }
    } catch (error) {
      if (alive.current) setPickError(toCommandError(error).message);
    } finally {
      if (alive.current) setPicking(false);
    }
  }

  async function runVerify() {
    if (!file) return;
    const requestId = newRequestId();
    requests.current.push(requestId);
    setVerify({ status: "running", requestId });
    try {
      const report = await backupVerify(file.handle, requestId);
      if (alive.current) setVerify({ status: "ok", report });
    } catch (error) {
      if (alive.current) setVerify({ status: "error", message: toCommandError(error).message });
    }
  }

  async function start() {
    if (!canRestore || !file || !target.container || !m) return;
    dispatch({ type: "start" });
    try {
      const opId = await backupRestore(
        file.handle,
        {
          session: target.sessionId,
          options: {
            container: target.container,
            content,
            objects: chosen.length === m.objects.length ? [] : chosen,
            dropExisting,
            truncateBeforeLoad: truncate,
            verifyFirst,
            confirmDestructive: destructive && acknowledged,
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

  // --- views ------------------------------------------------------------------------------

  function fileCard(): ReactNode {
    return (
      <div className={`transfer-file${file ? " transfer-file--picked" : ""}`}>
        <div className="transfer-file__text">
          {file ? (
            <span className="transfer-file__name" data-testid="restore-file">
              {file.name}
              {file.size !== undefined ? (
                <span className="transfer-file__size">{` · ${formatBytes(file.size)}`}</span>
              ) : null}
            </span>
          ) : (
            <span className="transfer-file__title">No backup chosen</span>
          )}
          <span className="transfer-file__hint">An Oxbase backup archive (.oxbackup).</span>
        </div>
        <button
          type="button"
          className={`button ${file ? "" : "button--primary"}`}
          data-testid="restore-choose"
          disabled={picking || running}
          onClick={() => void pick()}
        >
          {picking ? <LoaderCircle {...ICON} className="spin" /> : <FolderOpen {...ICON} />}
          {file ? "Choose another…" : "Choose backup…"}
        </button>
      </div>
    );
  }

  function manifestView(): ReactNode {
    if (manifest.status === "loading") {
      return (
        <p className="transfer-step__status" role="status">
          <LoaderCircle {...ICON_SM} className="spin" />
          Reading the manifest…
        </p>
      );
    }
    if (manifest.status === "error") {
      return (
        <Note tone="error" title="Not a usable backup" testId="restore-manifest-error">
          {manifest.message}
        </Note>
      );
    }
    if (!m) return null;
    return (
      <>
        <dl className="transfer-summary" data-testid="restore-manifest">
          <dt>Engine</dt>
          <dd>{`${ENGINE_INFO[m.engine]?.label ?? m.engine} ${m.serverVersion}`}</dd>
          <dt>From</dt>
          <dd>{containerLabel(m.source.container)}</dd>
          <dt>Created</dt>
          <dd>{`${formatDateTime(m.createdAt)} by Oxbase ${m.oxbaseVersion}`}</dd>
          <dt>Content</dt>
          <dd>{m.content === "structure_only" ? "Structure only" : "Structure and data"}</dd>
        </dl>
        <WarningList title="Noted when it was written" items={m.warnings} />
        {verifyOnly ? null : (
          <ul className="sync-checklist sync-checklist--short" aria-label="Objects in the backup">
            {m.order
              .map((i) => m.objects.find((o) => o.index === i))
              .filter((o) => o !== undefined)
              .map((o) => (
                <li key={o.index}>
                  <label className="checkbox">
                    <input
                      type="checkbox"
                      disabled={running}
                      checked={!excluded.includes(o.index)}
                      onChange={(e) =>
                        setExcluded(
                          e.target.checked
                            ? excluded.filter((x) => x !== o.index)
                            : [...excluded, o.index],
                        )
                      }
                    />
                    <span className="transfer-table__name">{qualifiedName(o.ref)}</span>
                    <span className="sync__meta">
                      {`${o.kind.replace(/_/g, " ")}${o.data ? ` · ${plural(o.data.rows, "row", "rows")}` : ""}`}
                    </span>
                  </label>
                </li>
              ))}
          </ul>
        )}
      </>
    );
  }

  function verifyView(): ReactNode {
    if (!file || manifest.status !== "ok") return null;
    return (
      <div className="sync-verify">
        <button
          type="button"
          className="button"
          data-testid="restore-verify"
          disabled={verify.status === "running" || running}
          onClick={() => void runVerify()}
        >
          {verify.status === "running" ? (
            <LoaderCircle {...ICON} className="spin" />
          ) : (
            <ShieldCheck {...ICON} />
          )}
          Verify checksums
        </button>
        {verify.status === "ok" ? (
          verify.report.ok ? (
            <Note tone="ok" title="Archive OK" testId="verify-result" role="status">
              {`${plural(verify.report.entriesChecked, "entry", "entries")} and ${formatBytes(verify.report.bytesChecked)} checked; every checksum and row count matches.`}
            </Note>
          ) : (
            <Note tone="error" title="The archive is damaged" testId="verify-result">
              <ul className="banner__list">
                {(verify.report.problems ?? [])
                  .filter((p) => !p.ok)
                  .map((p) => (
                    <li key={p.entry}>{`${p.entry}: ${p.problem ?? "does not match"}`}</li>
                  ))}
              </ul>
            </Note>
          )
        ) : null}
        {verify.status === "error" ? (
          <Note tone="error" title="Could not verify" testId="verify-result">
            {verify.message}
          </Note>
        ) : null}
      </div>
    );
  }

  function targetForm(): ReactNode {
    if (verifyOnly || !m) return null;
    return (
      <fieldset className="fieldset sync-restore">
        <legend className="transfer-label">Restore into</legend>
        <ContainerPicker
          label="Target"
          sessions={sameEngine}
          value={target}
          disabled={running}
          testId="restore-target"
          emptyHint={`Open a ${ENGINE_INFO[m.engine]?.label ?? m.engine} connection: a backup restores into the same engine.`}
          onChange={setTarget}
        />
        <p className="field__hint">The database or schema must exist already.</p>
        <div className="field">
          <label htmlFor={`${id}-content`}>Restore</label>
          <select
            id={`${id}-content`}
            value={content}
            disabled={running}
            onChange={(e) => setContent(e.target.value as RestoreContent)}
          >
            <option value="structure_and_data" disabled={m.content === "structure_only"}>
              Structure and data
            </option>
            <option value="structure_only">Structure only</option>
            <option value="data_only" disabled={m.content === "structure_only"}>
              Data only (into existing tables)
            </option>
          </select>
        </div>
        <label className="checkbox">
          <input
            type="checkbox"
            checked={dropExisting}
            disabled={running}
            onChange={(e) => setDropExisting(e.target.checked)}
          />
          Replace objects that already exist (drop them first)
        </label>
        <label className="checkbox">
          <input
            type="checkbox"
            checked={truncate}
            disabled={running || content === "structure_only"}
            onChange={(e) => setTruncate(e.target.checked)}
          />
          Empty tables before loading their rows
        </label>
        <label className="checkbox">
          <input
            type="checkbox"
            checked={verifyFirst}
            disabled={running}
            onChange={(e) => setVerifyFirst(e.target.checked)}
          />
          Verify the archive before restoring
        </label>
        {destructive ? (
          <>
            <Note tone="error" title="Destructive restore">
              {dropExisting && truncate
                ? "Existing objects are dropped and tables emptied in the target."
                : dropExisting
                  ? "Existing objects with the same names are dropped in the target."
                  : "Existing target tables are emptied before the load."}
            </Note>
            <Acknowledge
              checked={acknowledged}
              onChange={setAcknowledged}
              testId="restore-acknowledge"
            >
              I understand that the target's objects and rows can be lost.
            </Acknowledge>
          </>
        ) : null}
      </fieldset>
    );
  }

  function runView(): ReactNode {
    if (run.status === "idle") return null;
    const p = run.progress;
    const r = run.report;
    return (
      <div className="sync-run" data-testid="restore-progress">
        <p className="transfer-step__status" role="status" data-testid="restore-status">
          {running ? <LoaderCircle {...ICON_SM} className="spin" /> : null}
          {run.status === "starting"
            ? "Starting…"
            : run.status === "running"
              ? `Restoring (${p?.phase ?? "starting"})${p?.current ? ` · ${qualifiedName(p.current)}` : ""}`
              : run.status === "finished"
                ? "Restore finished"
                : run.status === "failed"
                  ? "Restore failed"
                  : "Restore cancelled"}
        </p>
        {running ? (
          <RunBar
            done={p ? (p.rowsTotal > 0 ? p.rows : p.objectsDone) : 0}
            total={p ? (p.rowsTotal > 0 ? p.rowsTotal : p.objectsTotal) : null}
            label="Restore progress"
          />
        ) : null}
        {run.status === "finished" && r ? (
          <Note tone="ok" title="Restored" testId="restore-result" role="status">
            {`${plural(r.objects.filter((o) => o.status !== "not_run").length, "object", "objects")}, ${plural(r.rows, "row", "rows")} in ${formatMs(r.elapsedMs)}.`}
          </Note>
        ) : null}
        {run.status === "failed" ? (
          <Note tone="error" title="The restore failed" testId="restore-result">
            {`${run.error?.message ?? ""}${r?.leftBehind ? ` ${r.leftBehind}` : ""}`}
          </Note>
        ) : null}
        {run.status === "cancelled" ? (
          <Note tone="warning" title="Restore cancelled" testId="restore-result">
            {r?.leftBehind || "Objects restored before the cancel stay."}
          </Note>
        ) : null}
        <WarningList title="Warnings" items={r?.warnings} />
        {r && r.objects.length > 0 ? (
          <table className="transfer-table" aria-label="Restored objects">
            <thead>
              <tr>
                <th scope="col">Object</th>
                <th scope="col">Result</th>
                <th scope="col">Rows</th>
              </tr>
            </thead>
            <tbody>
              {r.objects.map((o) => (
                <tr key={o.index}>
                  <th scope="row" className="transfer-table__name">
                    {qualifiedName(o.object)}
                  </th>
                  <td>
                    <span className={`badge badge--${RESTORED_TONE[o.status]}`}>
                      {o.status.replace(/_/g, " ")}
                    </span>
                    {o.error ? <p className="field__error">{o.error}</p> : null}
                  </td>
                  <td className="transfer-table__num">{o.rows.toLocaleString("en-US")}</td>
                </tr>
              ))}
            </tbody>
          </table>
        ) : null}
      </div>
    );
  }

  let footer: ReactNode;
  if (running) {
    footer = (
      <button
        type="button"
        className="button button--danger"
        data-testid="restore-cancel"
        disabled={run.cancelling || run.status === "starting"}
        onClick={cancel}
      >
        <CircleX {...ICON} />
        {run.cancelling ? "Cancelling…" : "Cancel restore"}
      </button>
    );
  } else if (verifyOnly || ended(run.status)) {
    footer = (
      <button type="button" className="button button--primary" onClick={onClose}>
        {ended(run.status) ? <CircleCheck {...ICON} /> : null}
        Close
      </button>
    );
  } else {
    footer = (
      <>
        <button type="button" className="button modal__footer-start" onClick={close}>
          Cancel
        </button>
        <button
          type="button"
          className={`button ${destructive ? "button--danger" : "button--primary"}`}
          data-testid="restore-start"
          disabled={!canRestore}
          onClick={() => void start()}
        >
          <ArchiveRestore {...ICON} />
          {chosen.length > 0 && m && chosen.length < m.objects.length
            ? `Restore ${plural(chosen.length, "object", "objects")}`
            : "Restore"}
        </button>
      </>
    );
  }

  return (
    <Modal
      title={verifyOnly ? "Verify a backup" : "Restore a backup"}
      onClose={close}
      className="modal--transfer-narrow sync-restore-dialog"
      footer={footer}
    >
      {fileCard()}
      {pickError ? <Note tone="error">{pickError}</Note> : null}
      {run.status === "idle" ? (
        <>
          {manifestView()}
          {verifyView()}
          {targetForm()}
        </>
      ) : (
        runView()
      )}
    </Modal>
  );
}
