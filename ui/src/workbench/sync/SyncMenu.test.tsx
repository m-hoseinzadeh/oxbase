import { fireEvent, render, screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { refreshSessions, resetConnectionsStore } from "../../connections/store";
import { mockCore } from "../../test/m8Fixtures";
import { setDockviewApi } from "../layout";
import { SyncHost } from "./SyncHost";
import { SyncMenu } from "./SyncMenu";
import { resetSyncStore } from "./store";

vi.mock("@tauri-apps/api/core", async () => (await import("../../test/tauriMock")).tauriCore);

describe("SyncMenu", () => {
  beforeEach(async () => {
    resetConnectionsStore();
    resetSyncStore();
    mockCore();
    await refreshSessions();
  });

  it("opens the transfer wizard and the backup dialogs", () => {
    render(
      <>
        <SyncMenu />
        <SyncHost />
      </>,
    );
    fireEvent.click(screen.getByTestId("app-transfer"));
    expect(screen.getByRole("dialog", { name: "Data transfer" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("dialog")).toBeNull();

    fireEvent.click(screen.getByTestId("app-backup"));
    const menu = screen.getByRole("menu", { name: "Backup" });
    fireEvent.click(within(menu).getByRole("menuitem", { name: "Verify a backup…" }));
    expect(screen.getByRole("dialog", { name: "Verify a backup" })).toBeInTheDocument();
  });

  it("opens the sync views as workbench tabs", () => {
    const addPanel = vi.fn();
    setDockviewApi({ panels: [], addPanel } as never);
    render(<SyncMenu />);
    fireEvent.click(screen.getByTestId("app-compare"));
    fireEvent.click(screen.getByRole("menuitem", { name: "Structure sync" }));
    fireEvent.click(screen.getByTestId("app-compare"));
    fireEvent.click(screen.getByRole("menuitem", { name: "Data sync" }));
    expect(addPanel.mock.calls.map((c) => [c[0].component, c[0].title])).toEqual([
      ["structureSync", "Structure sync"],
      ["dataSync", "Data sync"],
    ]);
    setDockviewApi(null);
  });
});
