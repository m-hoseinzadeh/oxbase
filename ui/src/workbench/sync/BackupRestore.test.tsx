import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type {
  BackupEvent,
  BackupRequest,
  FileFilter,
  Manifest,
  RestoreEvent,
  RestoreRequest,
} from "../../api";
import { refreshSessions, resetConnectionsStore } from "../../connections/store";
import {
  callsOf,
  channels,
  M8_SRC,
  M8_TGT,
  MYSQL,
  mockCore,
  PG_SOURCE,
  PG_TARGET,
  requestOf,
  table,
} from "../../test/m8Fixtures";
import { BackupDialog } from "./BackupDialog";
import { RestoreDialog } from "./RestoreDialog";
import { resetSyncStore } from "./store";

vi.mock("@tauri-apps/api/core", async () => (await import("../../test/tauriMock")).tauriCore);

const MANIFEST: Manifest = {
  format: "oxbase-backup",
  formatVersion: 1,
  oxbaseVersion: "0.8.0",
  createdAt: "2026-11-03T09:00:00Z",
  engine: "postgres",
  serverVersion: "18.6",
  source: { container: M8_SRC },
  content: "structure_and_data",
  compression: { codec: "zstd" },
  objects: [
    {
      index: 0,
      kind: "table",
      ref: table(M8_SRC, "customers"),
      definition: "objects/0000.json",
      data: { entry: "data/0000.arrows", rows: 12, columns: ["id", "email"] },
    },
    {
      index: 1,
      kind: "view",
      ref: table(M8_SRC, "active_customers", "view"),
      definition: "objects/0001.json",
    },
  ],
  order: [0, 1],
  entries: {},
};

describe("BackupDialog", () => {
  beforeEach(() => {
    resetConnectionsStore();
    resetSyncStore();
    mockCore({
      file_pick_save: (a) => ({
        handle: "w-1",
        name: a.defaultName,
        access: "write",
        exists: false,
      }),
      backup_run: () => "bk-1",
      backup_cancel: () => true,
    });
  });

  it("backs up a schema into an .oxbackup file and reports it", async () => {
    await refreshSessions();
    render(
      <BackupDialog
        init={{ source: { sessionId: PG_SOURCE.id, container: M8_SRC } }}
        onClose={vi.fn()}
      />,
    );
    const start = screen.getByTestId("backup-start");
    await waitFor(() => expect(start).toBeEnabled());
    fireEvent.change(screen.getByLabelText("Content"), { target: { value: "structure_only" } });
    await act(async () => fireEvent.click(start));
    await waitFor(() => expect(callsOf("backup_run")).toHaveLength(1));
    const save = callsOf("file_pick_save")[0];
    expect(save?.defaultName).toMatch(/^oxbase_test-m8_src-\d{8}-\d{4}\.oxbackup$/);
    expect((save?.filters as FileFilter[] | undefined)?.[0]?.extensions).toEqual(["oxbackup"]);
    expect(callsOf("backup_run")[0]?.file).toBe("w-1");
    expect(requestOf<BackupRequest>("backup_run", 0)).toEqual({
      session: PG_SOURCE.id,
      spec: { container: M8_SRC, content: "structure_only", compression: { codec: "zstd" } },
    });
    const send = channels.backup_run as (e: BackupEvent) => void;
    send({ type: "started", backupId: "bk-1", objects: 2 });
    send({
      type: "progress",
      progress: {
        objectsDone: 1,
        objectsTotal: 2,
        current: table(M8_SRC, "customers"),
        rows: 12,
        bytes: 3000,
        elapsedMs: 40,
      },
    });
    expect(screen.getByTestId("backup-status")).toHaveTextContent("m8_src.customers");
    send({ type: "finished", report: { manifest: MANIFEST, bytes: 4096, elapsedMs: 80 } });
    expect(await screen.findByTestId("backup-result")).toHaveTextContent("2 objects, 4.10 KB");
  });

  it("backs up chosen tables only and cancels", async () => {
    await refreshSessions();
    render(
      <BackupDialog
        init={{ source: { sessionId: PG_SOURCE.id, container: M8_SRC } }}
        onClose={vi.fn()}
      />,
    );
    const start = screen.getByTestId("backup-start");
    await waitFor(() => expect(start).toBeEnabled());
    fireEvent.click(screen.getByLabelText("Chosen tables and views"));
    const list = await screen.findByRole("list", { name: "Objects to back up" });
    expect(start).toBeDisabled();
    fireEvent.click(within(list).getByLabelText(/m8_src\.orders/));
    await act(async () => fireEvent.click(start));
    await waitFor(() => expect(callsOf("backup_run")).toHaveLength(1));
    expect(requestOf<BackupRequest>("backup_run", 0).spec.objects).toEqual([
      table(M8_SRC, "orders"),
    ]);
    const send = channels.backup_run as (e: BackupEvent) => void;
    send({ type: "started", backupId: "bk-1", objects: 1 });
    fireEvent.click(screen.getByTestId("backup-cancel"));
    expect(callsOf("backup_cancel")[0]).toEqual({ operationId: "bk-1" });
    send({ type: "cancelled", elapsedMs: 10 });
    expect(await screen.findByTestId("backup-result")).toHaveTextContent(
      "No archive was left behind",
    );
  });
});

describe("RestoreDialog", () => {
  beforeEach(() => {
    resetConnectionsStore();
    resetSyncStore();
    mockCore({
      file_pick_open: () => ({
        handle: "r-1",
        name: "m8.oxbackup",
        access: "read",
        size: 5000,
        exists: true,
      }),
      backup_list: () => MANIFEST,
      backup_verify: () => ({ ok: true, entriesChecked: 5, bytesChecked: 5000, problems: [] }),
      backup_restore: () => "rs-1",
      backup_cancel: () => true,
    });
  });

  it("reads the manifest, verifies and restores into the same engine with confirmation", async () => {
    await refreshSessions();
    const { unmount } = render(<RestoreDialog init={{}} onClose={vi.fn()} />);
    await act(async () => fireEvent.click(screen.getByTestId("restore-choose")));
    expect(await screen.findByTestId("restore-manifest")).toHaveTextContent("PostgreSQL 18.6");
    expect(screen.getByTestId("restore-manifest")).toHaveTextContent("oxbase_test.m8_src");
    expect(
      (callsOf("file_pick_open")[0]?.filters as FileFilter[] | undefined)?.[0]?.extensions,
    ).toEqual(["oxbackup"]);

    fireEvent.click(screen.getByTestId("restore-verify"));
    expect(await screen.findByTestId("verify-result")).toHaveTextContent("Archive OK");
    expect(callsOf("backup_verify")[0]).toMatchObject({ file: "r-1" });

    // Only connections of the backup's engine are offered.
    const connection = screen.getByLabelText("Target connection");
    const offered = within(connection)
      .getAllByRole("option")
      .map((o) => o.textContent);
    expect(offered).not.toContain(`${MYSQL.profile_name} (MySQL)`);
    fireEvent.change(connection, { target: { value: PG_TARGET.id } });
    fireEvent.change(await screen.findByLabelText("Target schema"), {
      target: { value: "m8_tgt" },
    });
    const start = screen.getByTestId("restore-start");
    await waitFor(() => expect(start).toBeEnabled());

    fireEvent.click(screen.getByLabelText(/active_customers/));
    fireEvent.click(screen.getByLabelText("Replace objects that already exist (drop them first)"));
    expect(start).toBeDisabled();
    fireEvent.click(screen.getByTestId("restore-acknowledge"));
    await waitFor(() => expect(start).toBeEnabled());
    await act(async () => fireEvent.click(start));
    await waitFor(() => expect(callsOf("backup_restore")).toHaveLength(1));
    expect(callsOf("backup_restore")[0]?.file).toBe("r-1");
    expect(requestOf<RestoreRequest>("backup_restore", 0)).toEqual({
      session: PG_TARGET.id,
      options: {
        container: M8_TGT,
        content: "structure_and_data",
        objects: [0],
        dropExisting: true,
        truncateBeforeLoad: false,
        verifyFirst: true,
        confirmDestructive: true,
      },
    });
    const send = channels.backup_restore as (e: RestoreEvent) => void;
    send({ type: "started", restoreId: "rs-1", manifest: MANIFEST });
    send({
      type: "progress",
      progress: {
        phase: "data",
        objectsDone: 0,
        objectsTotal: 1,
        current: table(M8_TGT, "customers"),
        rows: 6,
        rowsTotal: 12,
        elapsedMs: 10,
      },
    });
    expect(screen.getByTestId("restore-status")).toHaveTextContent("Restoring (data)");
    send({
      type: "finished",
      report: {
        objects: [{ index: 0, object: table(M8_TGT, "customers"), status: "replaced", rows: 12 }],
        rows: 12,
        leftBehind: "",
        elapsedMs: 30,
      },
    });
    expect(await screen.findByTestId("restore-result")).toHaveTextContent("1 object, 12 rows");
    unmount();
    expect(callsOf("file_release")).toEqual([{ handle: "r-1" }]);
  });

  it("verifies only, and shows damaged entries", async () => {
    mockCore({
      file_pick_open: () => ({ handle: "r-2", name: "bad.oxbackup", access: "read", exists: true }),
      backup_list: () => MANIFEST,
      backup_verify: () => ({
        ok: false,
        entriesChecked: 5,
        bytesChecked: 5000,
        problems: [{ entry: "data/0000.arrows", ok: false, problem: "SHA-256 does not match" }],
      }),
    });
    await refreshSessions();
    render(<RestoreDialog init={{ verifyOnly: true }} onClose={vi.fn()} />);
    expect(screen.getByRole("dialog", { name: "Verify a backup" })).toBeInTheDocument();
    await act(async () => fireEvent.click(screen.getByTestId("restore-choose")));
    fireEvent.click(await screen.findByTestId("restore-verify"));
    expect(await screen.findByTestId("verify-result")).toHaveTextContent(
      "data/0000.arrows: SHA-256 does not match",
    );
    expect(screen.queryByTestId("restore-start")).toBeNull();
    expect(screen.queryByLabelText("Target connection")).toBeNull();
  });

  it("shows why an archive cannot be read", async () => {
    mockCore({
      file_pick_open: () => ({ handle: "r-3", name: "new.oxbackup", access: "read", exists: true }),
      backup_list: () => {
        throw {
          category: "unsupported",
          message: "format version 2 is newer than this Oxbase reads",
        };
      },
    });
    await refreshSessions();
    render(<RestoreDialog init={{}} onClose={vi.fn()} />);
    await act(async () => fireEvent.click(screen.getByTestId("restore-choose")));
    expect(await screen.findByTestId("restore-manifest-error")).toHaveTextContent(
      "format version 2 is newer",
    );
  });
});
