import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { resetConnectionsStore } from "../connections/store";
import type { ConnectionProfile, SessionSummary } from "../connections/types";
import { StatusBar } from "../StatusBar";
import { tauriCore } from "../test/tauriMock";
import { ConnectionsPanel } from "./ConnectionsPanel";

vi.mock("@tauri-apps/api/core", async () => (await import("../test/tauriMock")).tauriCore);
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));

const PG: ConnectionProfile = {
  schema_version: 1,
  id: "0190f000-0000-7000-8000-000000000001",
  name: "Production",
  engine: "postgres",
  address: { kind: "network", host: "db.example.com" },
  user: "app",
  password: { source: "keychain", key: "connection/x/password" },
  tls: { mode: "verify_full" },
};
const LITE: ConnectionProfile = {
  schema_version: 1,
  id: "0190f000-0000-7000-8000-000000000002",
  name: "Scratch",
  engine: "sqlite",
  address: { kind: "file", path: "/tmp/scratch.db" },
  tls: { mode: "disable" },
};
const PROMPT: ConnectionProfile = {
  ...PG,
  id: "0190f000-0000-7000-8000-000000000003",
  name: "Staging",
  password: { source: "prompt" },
};

function session(profile: ConnectionProfile): SessionSummary {
  return {
    id: `s-${profile.id}`,
    profile_id: profile.id,
    profile_name: profile.name,
    engine: profile.engine,
    info: {
      server_version: { major: 18, minor: 6, patch: 0, build: 0, raw: "18.6" },
      tls_in_use: true,
      via_ssh: false,
      latency_ms: 5,
    },
    opened_at: "2026-10-31T10:00:00Z",
  };
}

let sessions: SessionSummary[] = [];
let profiles: ConnectionProfile[] = [];

function mockCore() {
  tauriCore.invoke.mockImplementation(async (cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "app_info":
        return { version: "0.0.1", platform: "linux", arch: "x86_64" };
      case "list_profiles":
        return profiles;
      case "list_open_connections":
        return sessions;
      case "open_connection": {
        const profile = profiles.find((p) => p.id === a.id);
        if (!profile) throw { category: "not_found", message: "gone" };
        if (profile.name === "Broken") throw { category: "timeout", message: "after 10 s" };
        sessions = [...sessions, session(profile)];
        return session(profile);
      }
      case "close_connection":
        sessions = sessions.filter((s) => s.id !== a.sessionId);
        return null;
      case "delete_profile":
        profiles = profiles.filter((p) => p.id !== a.id);
        return true;
      default:
        throw new Error(`unexpected ${cmd}`);
    }
  });
}

function renderPanel() {
  return render(
    <>
      <ConnectionsPanel />
      <StatusBar />
    </>,
  );
}

describe("ConnectionsPanel", () => {
  beforeEach(() => {
    resetConnectionsStore();
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReset().mockReturnValue(true);
    sessions = [];
    profiles = [PG, LITE, PROMPT];
    mockCore();
  });

  it("lists profiles with engine, location and TLS mode", async () => {
    renderPanel();
    const list = await screen.findByRole("listbox", { name: "Saved connections" });
    const options = within(list).getAllByRole("option");
    expect(options).toHaveLength(3);
    expect(options[0]).toHaveTextContent("Production");
    expect(options[0]).toHaveTextContent(
      "PostgreSQL · db.example.com:5432 · TLS: Verify certificate and host name",
    );
    expect(options[1]).toHaveTextContent("SQLite · /tmp/scratch.db · local file");
    expect(screen.getByRole("button", { name: "Edit" })).toBeDisabled();
  });

  it("connects with the keyboard and counts open connections in the status bar", async () => {
    renderPanel();
    const list = await screen.findByRole("listbox");
    expect(screen.getByTestId("open-connections")).toHaveTextContent("0 open connections");
    fireEvent.keyDown(list, { key: "ArrowDown" });
    expect(within(list).getAllByRole("option")[0]).toHaveAttribute("aria-selected", "true");
    fireEvent.keyDown(list, { key: "Enter" });
    await waitFor(() =>
      expect(screen.getByTestId("open-connections")).toHaveTextContent("1 open connection"),
    );
    expect(tauriCore.invoke).toHaveBeenCalledWith(
      "open_connection",
      expect.objectContaining({ id: PG.id, secrets: {} }),
    );
    expect(within(list).getAllByRole("option")[0]).toHaveTextContent("Connected");

    fireEvent.click(screen.getByRole("button", { name: "Disconnect" }));
    await waitFor(() =>
      expect(screen.getByTestId("open-connections")).toHaveTextContent("0 open connections"),
    );
  });

  it("asks for a password that is not saved", async () => {
    renderPanel();
    fireEvent.click(await screen.findByText("Staging"));
    fireEvent.click(screen.getByRole("button", { name: "Connect" }));
    const dialog = screen.getByRole("dialog", { name: "Connect to Staging" });
    const input = within(dialog).getByLabelText("Database password");
    expect(input).toHaveFocus();
    fireEvent.change(input, { target: { value: "typed-secret" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Connect" }));
    await waitFor(() =>
      expect(tauriCore.invoke).toHaveBeenCalledWith(
        "open_connection",
        expect.objectContaining({ id: PROMPT.id, secrets: { password: "typed-secret" } }),
      ),
    );
  });

  it("shows the category and message when connecting fails", async () => {
    profiles = [{ ...PG, name: "Broken" }];
    renderPanel();
    fireEvent.click(await screen.findByText("Broken"));
    fireEvent.click(screen.getByRole("button", { name: "Connect" }));
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("Timed out");
    expect(alert).toHaveTextContent("after 10 s");
  });

  it("cancels a connect that is still running", async () => {
    profiles = [{ ...PG, name: "Slow" }];
    let rejectOpen: ((reason: unknown) => void) | undefined;
    tauriCore.invoke.mockImplementation(async (cmd, args) => {
      const a = (args ?? {}) as Record<string, unknown>;
      switch (cmd) {
        case "list_profiles":
          return profiles;
        case "list_open_connections":
          return sessions;
        case "open_connection":
          return new Promise((_, reject) => {
            rejectOpen = reject;
          });
        case "cancel_request":
          rejectOpen?.({ category: "cancelled", message: "cancelled" });
          return a.requestId !== undefined;
        default:
          throw new Error(`unexpected ${cmd}`);
      }
    });
    renderPanel();
    fireEvent.click(await screen.findByText("Slow"));
    fireEvent.click(screen.getByRole("button", { name: "Connect" }));
    const cancel = await screen.findByRole("button", { name: "Cancel connecting" });
    expect(screen.getByRole("button", { name: "Connecting…" })).toBeDisabled();

    const openCall = tauriCore.invoke.mock.calls.find(([cmd]) => cmd === "open_connection");
    const requestId = (openCall?.[1] as { requestId?: string } | undefined)?.requestId;
    expect(requestId).toEqual(expect.any(String));

    fireEvent.click(cancel);
    expect(tauriCore.invoke).toHaveBeenCalledWith("cancel_request", { requestId });
    expect(await screen.findByRole("status")).toHaveTextContent(
      "Connecting to Slow was cancelled.",
    );
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Cancel connecting" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Connect" })).toBeEnabled();
  });

  it("deletes after confirmation", async () => {
    renderPanel();
    const list = await screen.findByRole("listbox");
    fireEvent.click(screen.getByText("Scratch"));
    fireEvent.keyDown(list, { key: "Delete" });
    const dialog = screen.getByRole("dialog", { name: "Delete connection" });
    expect(within(dialog).getByRole("button", { name: "Cancel" })).toHaveFocus();
    fireEvent.click(within(dialog).getByRole("button", { name: "Delete" }));
    await waitFor(() => expect(screen.queryByText("Scratch")).not.toBeInTheDocument());
    expect(tauriCore.invoke).toHaveBeenCalledWith("delete_profile", { id: LITE.id });
  });

  it("opens the edit dialog for the selected profile", async () => {
    renderPanel();
    fireEvent.click(await screen.findByText("Production"));
    fireEvent.click(screen.getByRole("button", { name: "Edit" }));
    const dialog = screen.getByRole("dialog", { name: "Edit connection: Production" });
    expect(within(dialog).getByLabelText("Host")).toHaveValue("db.example.com");
    expect(
      within(dialog).getByText(
        "A password is saved. Leave blank to keep it, unless you change where it connects.",
      ),
    ).toBeVisible();
  });
});
