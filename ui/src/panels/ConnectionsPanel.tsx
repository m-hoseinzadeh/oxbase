import {
  CircleAlert,
  Database,
  Info,
  Loader2,
  LockKeyhole,
  Pencil,
  Plug,
  Plus,
  Trash2,
  Unplug,
} from "lucide-react";
import { type FormEvent, type KeyboardEvent, useEffect, useId, useRef, useState } from "react";
import { ICON, ICON_LG, ICON_SM } from "../components/icons";
import { Modal } from "../components/Modal";
import {
  cancelRequest,
  closeConnection,
  deleteProfile,
  newRequestId,
  openConnection,
  toCommandError,
} from "../connections/api";
import { CATEGORY_LABEL, ConnectionDialog, TLS_MODES } from "../connections/ConnectionDialog";
import { EngineBadge } from "../connections/EngineBadge";
import { promptedSecrets } from "../connections/form";
import { onNewConnectionRequest } from "../connections/requests";
import { refreshAll, refreshProfiles, refreshSessions, useConnections } from "../connections/store";
import {
  type CommandError,
  type ConnectionProfile,
  ENGINE_INFO,
  profileLocation,
  type SecretInput,
} from "../connections/types";

const SECRET_LABELS: Record<keyof SecretInput, string> = {
  password: "Database password",
  ssh_password: "SSH password",
  ssh_key_passphrase: "SSH key passphrase",
};

type DialogState =
  | { kind: "none" }
  | { kind: "edit"; profile?: ConnectionProfile }
  | { kind: "delete"; profile: ConnectionProfile }
  | { kind: "prompt"; profile: ConnectionProfile; fields: Array<keyof SecretInput> };

function tlsLabel(profile: ConnectionProfile): string {
  if (profile.engine === "sqlite") return "local file";
  const mode = TLS_MODES.find((m) => m.value === profile.tls.mode)?.label ?? profile.tls.mode;
  return `TLS: ${mode}`;
}

function PromptSecretsDialog({
  profile,
  fields,
  onCancel,
  onSubmit,
}: {
  profile: ConnectionProfile;
  fields: Array<keyof SecretInput>;
  onCancel: () => void;
  onSubmit: (secrets: SecretInput) => void;
}) {
  const [values, setValues] = useState<SecretInput>({});
  const formId = useId();
  function submit(event: FormEvent) {
    event.preventDefault();
    onSubmit(values);
  }
  return (
    <Modal
      title={`Connect to ${profile.name}`}
      onClose={onCancel}
      footer={
        <>
          <button type="button" className="button" onClick={onCancel}>
            Cancel
          </button>
          <button
            type="submit"
            form={formId}
            className="button button--primary"
            data-testid="password-prompt-submit"
          >
            Connect
          </button>
        </>
      }
    >
      <form id={formId} className="form-stack" onSubmit={submit} data-testid="password-prompt">
        <p className="callout callout--muted">
          <LockKeyhole {...ICON_SM} className="callout__icon" />
          <span className="callout__text">
            These are not saved. They are used for this connection only.
          </span>
        </p>
        {fields.map((field, index) => (
          <div className="field" key={field}>
            <label htmlFor={`prompt-${field}`}>{SECRET_LABELS[field]}</label>
            <input
              id={`prompt-${field}`}
              type="password"
              autoComplete="off"
              {...(index === 0 ? { "data-autofocus": true } : {})}
              value={values[field] ?? ""}
              onChange={(e) => setValues((v) => ({ ...v, [field]: e.target.value }))}
            />
          </div>
        ))}
      </form>
    </Modal>
  );
}

export function ConnectionsPanel() {
  const { profiles, profilesStatus, profilesError, sessions } = useConnections();
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [dialog, setDialog] = useState<DialogState>({ kind: "none" });
  const [busyId, setBusyId] = useState<string | null>(null);
  // The connect in flight, so it can be cancelled (toolbar button, or leaving the panel).
  const [pendingOpen, setPendingOpen] = useState<{ profileId: string; requestId: string } | null>(
    null,
  );
  const pendingRequest = useRef<string | null>(null);
  const [actionError, setActionError] = useState<CommandError | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const listId = useId();

  useEffect(() => {
    void refreshAll();
    return () => {
      if (pendingRequest.current) void cancelRequest(pendingRequest.current).catch(() => {});
    };
  }, []);

  const selected = profiles.find((p) => p.id === selectedId) ?? null;
  const openSessions = (profile: ConnectionProfile) =>
    sessions.filter((s) => s.profile_id === profile.id);
  const selectedOpen = selected ? openSessions(selected).length > 0 : false;

  async function connect(profile: ConnectionProfile, secrets: SecretInput = {}) {
    setDialog({ kind: "none" });
    setBusyId(profile.id);
    setActionError(null);
    setNotice(null);
    const requestId = newRequestId();
    pendingRequest.current = requestId;
    setPendingOpen({ profileId: profile.id, requestId });
    try {
      await openConnection(profile.id, secrets, requestId);
    } catch (error) {
      const err = toCommandError(error);
      if (err.category === "cancelled") setNotice(`Connecting to ${profile.name} was cancelled.`);
      else setActionError(err);
    } finally {
      pendingRequest.current = null;
      setPendingOpen(null);
      setBusyId(null);
      await refreshSessions();
    }
  }

  function cancelConnect() {
    if (pendingOpen) void cancelRequest(pendingOpen.requestId).catch(() => {});
  }

  function startConnect(profile: ConnectionProfile) {
    const fields = promptedSecrets(profile);
    if (fields.length > 0) setDialog({ kind: "prompt", profile, fields });
    else void connect(profile);
  }

  async function disconnect(profile: ConnectionProfile) {
    setActionError(null);
    setNotice(null);
    try {
      for (const session of openSessions(profile)) await closeConnection(session.id);
    } catch (error) {
      setActionError(toCommandError(error));
    } finally {
      await refreshSessions();
    }
  }

  async function confirmDelete(profile: ConnectionProfile) {
    setDialog({ kind: "none" });
    setActionError(null);
    try {
      await deleteProfile(profile.id);
      if (selectedId === profile.id) setSelectedId(null);
    } catch (error) {
      setActionError(toCommandError(error));
    } finally {
      await refreshProfiles();
    }
  }

  function onListKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (profiles.length === 0) return;
    const index = profiles.findIndex((p) => p.id === selectedId);
    let next: number | undefined;
    switch (event.key) {
      case "ArrowDown":
        next = Math.min(index + 1, profiles.length - 1);
        break;
      case "ArrowUp":
        next = index < 0 ? 0 : Math.max(index - 1, 0);
        break;
      case "Home":
        next = 0;
        break;
      case "End":
        next = profiles.length - 1;
        break;
      case "Enter":
        if (selected) {
          event.preventDefault();
          if (selectedOpen) void disconnect(selected);
          else startConnect(selected);
        }
        return;
      case "Delete":
        if (selected) {
          event.preventDefault();
          setDialog({ kind: "delete", profile: selected });
        }
        return;
      default:
        return;
    }
    event.preventDefault();
    const target = profiles[next];
    if (target) setSelectedId(target.id);
  }

  // The app bar and welcome screen ask for a new connection through the request bus.
  useEffect(() => onNewConnectionRequest(() => setDialog({ kind: "edit" })), []);

  const noCore = profilesStatus === "error" && profilesError?.category === "unsupported";

  return (
    <section className="panel panel--connections" aria-label="Connections">
      <header className="panel__header connections__header">
        <span>Saved connections</span>
        {profiles.length > 0 ? (
          <span className="connections__count" aria-hidden="true">
            {profiles.length}
          </span>
        ) : null}
        <div className="panel__header-actions">
          <button
            type="button"
            className="icon-button"
            aria-label="New"
            title="New connection"
            onClick={() => setDialog({ kind: "edit" })}
          >
            <Plus {...ICON} />
          </button>
          <button
            type="button"
            className="icon-button"
            aria-label="Edit"
            title="Edit connection"
            disabled={!selected}
            onClick={() => selected && setDialog({ kind: "edit", profile: selected })}
          >
            <Pencil {...ICON} />
          </button>
          <button
            type="button"
            className="icon-button icon-button--danger"
            aria-label="Delete"
            title="Delete connection (Delete)"
            disabled={!selected}
            onClick={() => selected && setDialog({ kind: "delete", profile: selected })}
          >
            <Trash2 {...ICON} />
          </button>
        </div>
      </header>

      {notice ? (
        <div className="callout" role="status">
          <Info {...ICON_SM} className="callout__icon" />
          <p className="callout__text">{notice}</p>
        </div>
      ) : null}

      {actionError ? (
        <div className="callout callout--danger" role="alert">
          <CircleAlert {...ICON_SM} className="callout__icon" />
          <div className="callout__text">
            <p className="callout__title">{CATEGORY_LABEL[actionError.category]}</p>
            <p className="callout__message">{actionError.message}</p>
          </div>
        </div>
      ) : null}

      {profilesStatus === "error" && !noCore ? (
        <div className="callout callout--danger" role="alert">
          <CircleAlert {...ICON_SM} className="callout__icon" />
          <div className="callout__text">
            <p className="callout__title">Could not load connections</p>
            <p className="callout__message">{profilesError?.message}</p>
          </div>
        </div>
      ) : null}

      {profiles.length === 0 ? (
        profilesStatus === "loading" ? (
          <div className="connection-list connection-list--loading" aria-busy="true">
            <span className="visually-hidden">Loading connections…</span>
            {[0, 1, 2].map((i) => (
              <div key={i} className="connection-list__skeleton" aria-hidden="true">
                <span className="skeleton connection-list__skeleton-tile" />
                <span className="connection-list__skeleton-text">
                  <span className="skeleton connection-list__skeleton-line" />
                  <span className="skeleton connection-list__skeleton-line connection-list__skeleton-line--short" />
                </span>
              </div>
            ))}
          </div>
        ) : (
          <div className="empty-state">
            <span className="empty-state__icon" aria-hidden="true">
              <Database {...ICON_LG} />
            </span>
            <p className="empty-state__title">No connections yet</p>
            <p className="empty-state__hint">
              {noCore
                ? "Saved connections are available in the desktop app."
                : "Add a PostgreSQL, MySQL, MariaDB, SQLite or SQL Server connection."}
            </p>
            {noCore ? null : (
              <button
                type="button"
                className="button button--primary"
                onClick={() => setDialog({ kind: "edit" })}
              >
                <Plus {...ICON} />
                New connection
              </button>
            )}
          </div>
        )
      ) : (
        <div
          id={listId}
          className="connection-list"
          data-testid="connection-list"
          role="listbox"
          aria-label="Saved connections"
          tabIndex={0}
          aria-activedescendant={selected ? `conn-item-${selected.id}` : undefined}
          onKeyDown={onListKeyDown}
        >
          {profiles.map((profile) => {
            const open = openSessions(profile).length > 0;
            return (
              // biome-ignore lint/a11y/useKeyWithClickEvents: the listbox handles the keyboard.
              <div
                key={profile.id}
                tabIndex={-1}
                id={`conn-item-${profile.id}`}
                role="option"
                data-testid="connection-item"
                aria-selected={profile.id === selectedId}
                className={`connection-list__item${open ? " connection-list__item--open" : ""}`}
                onClick={() => setSelectedId(profile.id)}
                onDoubleClick={() => (open ? undefined : startConnect(profile))}
              >
                <EngineBadge engine={profile.engine} />
                <span className="connection-list__text">
                  <span className="connection-list__name">{profile.name}</span>
                  <span className="connection-list__meta">
                    {ENGINE_INFO[profile.engine].label} · {profileLocation(profile)}
                    {profile.ssh ? " · via SSH" : ""} · {tlsLabel(profile)}
                  </span>
                </span>
                {open ? (
                  <span className="connection-list__status">
                    <span className="status-dot" aria-hidden="true" />
                    Connected
                  </span>
                ) : busyId === profile.id ? (
                  <span className="connection-list__status connection-list__status--busy">
                    <Loader2 {...ICON_SM} className="spin" />
                    Connecting…
                  </span>
                ) : null}
              </div>
            );
          })}
        </div>
      )}

      {profiles.length > 0 || pendingOpen ? (
        <div
          className="toolbar connections__actions"
          role="toolbar"
          aria-label="Connection actions"
        >
          <button
            type="button"
            className="button button--primary connections__connect"
            data-testid="connect-button"
            disabled={!selected || busyId !== null}
            title={selected ? undefined : "Select a connection first"}
            onClick={() => {
              if (!selected) return;
              if (selectedOpen) void disconnect(selected);
              else startConnect(selected);
            }}
          >
            {selectedOpen ? (
              <Unplug {...ICON} />
            ) : busyId ? (
              <Loader2 {...ICON} className="spin" />
            ) : (
              <Plug {...ICON} />
            )}
            {selectedOpen ? "Disconnect" : busyId ? "Connecting…" : "Connect"}
          </button>
          {pendingOpen ? (
            <button type="button" className="button" onClick={cancelConnect}>
              Cancel connecting
            </button>
          ) : null}
        </div>
      ) : null}

      {dialog.kind === "edit" ? (
        <ConnectionDialog
          profile={dialog.profile}
          onClose={() => setDialog({ kind: "none" })}
          onSaved={(saved) => {
            setDialog({ kind: "none" });
            setSelectedId(saved.id);
            void refreshProfiles();
          }}
        />
      ) : null}

      {dialog.kind === "delete" ? (
        <Modal
          title="Delete connection"
          onClose={() => setDialog({ kind: "none" })}
          footer={
            <>
              <button
                type="button"
                className="button"
                data-autofocus
                onClick={() => setDialog({ kind: "none" })}
              >
                Cancel
              </button>
              <button
                type="button"
                className="button button--danger"
                onClick={() => void confirmDelete(dialog.profile)}
              >
                <Trash2 {...ICON} />
                Delete
              </button>
            </>
          }
        >
          <p>
            Delete <strong>{dialog.profile.name}</strong>? Its saved passwords are removed from the
            keychain. Open sessions stay open until you disconnect.
          </p>
        </Modal>
      ) : null}

      {dialog.kind === "prompt" ? (
        <PromptSecretsDialog
          profile={dialog.profile}
          fields={dialog.fields}
          onCancel={() => setDialog({ kind: "none" })}
          onSubmit={(secrets) => void connect(dialog.profile, secrets)}
        />
      ) : null}
    </section>
  );
}
