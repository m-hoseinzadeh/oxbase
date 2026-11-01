import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { tauriCore } from "../test/tauriMock";
import { ConnectionDialog } from "./ConnectionDialog";
import type { ConnectionInfo, ConnectionProfile } from "./types";

vi.mock("@tauri-apps/api/core", async () => (await import("../test/tauriMock")).tauriCore);
const dialogOpen = vi.fn<(options?: unknown) => Promise<string | null>>();
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: (o: unknown) => dialogOpen(o) }));

const INFO: ConnectionInfo = {
  server_version: { major: 18, minor: 6, patch: 0, build: 0, raw: "PostgreSQL 18.6" },
  tls_in_use: true,
  via_ssh: false,
  latency_ms: 42,
};

type Handler = (args: Record<string, unknown> | undefined) => unknown;

function mockCommands(handlers: Record<string, Handler>) {
  tauriCore.invoke.mockImplementation(async (cmd, args) => {
    const handler = handlers[cmd];
    if (!handler) throw new Error(`unexpected command ${cmd}`);
    return handler(args as Record<string, unknown> | undefined);
  });
}

function renderDialog(profile?: ConnectionProfile) {
  const onClose = vi.fn();
  const onSaved = vi.fn();
  render(<ConnectionDialog profile={profile} onClose={onClose} onSaved={onSaved} />);
  return { onClose, onSaved };
}

const field = (name: RegExp | string) => screen.getByLabelText(name);
const type = (name: RegExp | string, value: string) =>
  fireEvent.change(field(name), { target: { value } });

describe("ConnectionDialog validation", () => {
  beforeEach(() => {
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReset().mockReturnValue(true);
    dialogOpen.mockReset();
  });

  it("is an accessible modal that focuses the name field", () => {
    renderDialog();
    const dialog = screen.getByRole("dialog", { name: "New connection" });
    expect(dialog).toHaveAttribute("aria-modal", "true");
    expect(field("Connection name")).toHaveFocus();
    expect(screen.getByRole("tab", { name: "General" })).toHaveAttribute("aria-selected", "true");
    expect(field("Port")).toHaveValue("5432");
  });

  it("blocks saving without a name and explains why", async () => {
    renderDialog();
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    const name = field("Connection name");
    expect(name).toHaveAttribute("aria-invalid", "true");
    expect(name).toHaveAccessibleDescription("Enter a name for this connection.");
    await waitFor(() => expect(name).toHaveFocus());
    expect(tauriCore.invoke).not.toHaveBeenCalled();
  });

  it("rejects a bad port and clears the error once fixed", () => {
    renderDialog();
    type("Connection name", "local");
    type("Port", "99999");
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(screen.getByText("Port must be a number from 1 to 65535.")).toBeInTheDocument();
    type("Port", "5433");
    expect(screen.queryByText("Port must be a number from 1 to 65535.")).not.toBeInTheDocument();
  });

  it("jumps to the SSH tab when an SSH field is missing", async () => {
    renderDialog();
    type("Connection name", "via bastion");
    fireEvent.click(screen.getByRole("tab", { name: "SSH" }));
    fireEvent.click(screen.getByLabelText("Connect through an SSH tunnel"));
    fireEvent.click(screen.getByRole("tab", { name: "General" }));
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(screen.getByRole("tab", { name: "SSH" })).toHaveAttribute("aria-selected", "true");
    await waitFor(() => expect(field("SSH host")).toHaveFocus());
    expect(screen.getByText("Enter the SSH user name.")).toBeInTheDocument();
  });

  it("moves between tabs with the arrow keys", () => {
    renderDialog();
    const general = screen.getByRole("tab", { name: "General" });
    fireEvent.keyDown(general, { key: "ArrowRight" });
    const tls = screen.getByRole("tab", { name: "SSL/TLS" });
    expect(tls).toHaveAttribute("aria-selected", "true");
    expect(tls).toHaveFocus();
    fireEvent.keyDown(tls, { key: "End" });
    expect(screen.getByRole("tab", { name: "SSH" })).toHaveFocus();
  });

  it("explains each TLS mode in plain language", () => {
    renderDialog();
    fireEvent.click(screen.getByRole("tab", { name: "SSL/TLS" }));
    const mode = field("Encryption mode");
    expect(mode).toHaveAccessibleDescription(/when the server supports it/);
    fireEvent.change(mode, { target: { value: "verify_full" } });
    expect(screen.getByTestId("tls-help")).toHaveTextContent(/belongs to this server's host name/);
    fireEvent.change(mode, { target: { value: "disable" } });
    expect(screen.getByTestId("tls-help")).toHaveTextContent(/plain text/);
  });

  it("switches to a file picker for SQLite and disables network tabs", async () => {
    dialogOpen.mockResolvedValue("/data/app.db");
    renderDialog();
    fireEvent.change(field("Database type"), { target: { value: "sqlite" } });
    expect(screen.queryByLabelText("Host")).not.toBeInTheDocument();
    expect(screen.getByRole("tab", { name: "SSH" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Browse for the database file" }));
    await waitFor(() => expect(field("Database file")).toHaveValue("/data/app.db"));
    expect(dialogOpen).toHaveBeenCalledWith(expect.objectContaining({ multiple: false }));
  });
});

describe("ConnectionDialog test connection", () => {
  beforeEach(() => {
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReset().mockReturnValue(true);
  });

  it("shows server version, TLS, SSH and latency on success", async () => {
    mockCommands({ test_connection: () => INFO });
    renderDialog();
    type("Host", "db.internal");
    type("User name", "app");
    type("Password", "hunter2-marker");
    fireEvent.click(screen.getByRole("button", { name: "Test connection" }));

    const status = screen.getByRole("status");
    expect(await within(status).findByText("Connection succeeded")).toBeInTheDocument();
    expect(within(status).getByText("PostgreSQL 18.6")).toBeInTheDocument();
    expect(within(status).getByText("In use")).toBeInTheDocument();
    expect(within(status).getByText("No")).toBeInTheDocument();
    expect(within(status).getByText("42 ms")).toBeInTheDocument();

    const [cmd, args] = tauriCore.invoke.mock.calls[0] ?? [];
    expect(cmd).toBe("test_connection");
    const { profile, secrets, requestId } = args as {
      profile: ConnectionProfile;
      secrets: unknown;
      requestId: string;
    };
    expect(profile.address).toEqual({ kind: "network", host: "db.internal", port: 5432 });
    expect(profile.name).toBe("db.internal");
    expect(JSON.stringify(profile)).not.toContain("hunter2-marker");
    expect(secrets).toEqual({ password: "hunter2-marker" });
    expect(requestId).toMatch(/^req-/);
  });

  it("shows the error category and message on failure", async () => {
    mockCommands({
      test_connection: () => {
        throw { category: "auth", message: 'password authentication failed for user "app"' };
      },
    });
    renderDialog();
    fireEvent.click(screen.getByRole("button", { name: "Test connection" }));
    const status = screen.getByRole("status");
    expect(await within(status).findByText(/Connection failed/)).toBeInTheDocument();
    expect(within(status).getByTestId("error-category")).toHaveTextContent("Authentication failed");
    expect(
      within(status).getByText('password authentication failed for user "app"'),
    ).toBeInTheDocument();
  });

  it("can stop a running test", async () => {
    let finish: (value: ConnectionInfo) => void = () => {};
    mockCommands({
      test_connection: () => new Promise<ConnectionInfo>((resolve) => (finish = resolve)),
      cancel_request: () => true,
    });
    renderDialog();
    fireEvent.click(screen.getByRole("button", { name: "Test connection" }));
    expect(await screen.findByText("Testing connection…")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Stop test" }));
    await waitFor(() =>
      expect(tauriCore.invoke).toHaveBeenCalledWith("cancel_request", {
        requestId: expect.stringMatching(/^req-/),
      }),
    );
    finish(INFO);
  });

  it("does not test when the form is invalid", () => {
    renderDialog();
    type("Host", "");
    fireEvent.click(screen.getByRole("button", { name: "Test connection" }));
    expect(screen.getByText("Enter the server's host name or IP address.")).toBeInTheDocument();
    expect(tauriCore.invoke).not.toHaveBeenCalled();
  });
});

describe("ConnectionDialog save", () => {
  beforeEach(() => {
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReset().mockReturnValue(true);
  });

  it("sends secrets separately and hands back the saved profile", async () => {
    const saved = { id: "0190f000-0000-7000-8000-000000000001" };
    mockCommands({ save_profile: () => saved });
    const { onSaved } = renderDialog();
    type("Connection name", "prod");
    type("Password", "hunter2-marker");
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(onSaved).toHaveBeenCalledWith(saved));
    const [, args] = tauriCore.invoke.mock.calls[0] ?? [];
    const { profile, secrets } = args as { profile: ConnectionProfile; secrets: unknown };
    expect(profile.password).toEqual({ source: "keychain", key: "pending" });
    expect(secrets).toEqual({ password: "hunter2-marker" });
  });

  it("shows why the core refused to save", async () => {
    mockCommands({
      save_profile: () => {
        throw { category: "unsupported", message: "no keychain available" };
      },
    });
    const { onSaved } = renderDialog();
    type("Connection name", "prod");
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("Could not save: Not supported");
    expect(alert).toHaveTextContent("no keychain available");
    expect(onSaved).not.toHaveBeenCalled();
  });

  it("closes on Escape", () => {
    const { onClose } = renderDialog();
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" });
    expect(onClose).toHaveBeenCalled();
  });
});

describe("ConnectionDialog for SQL Server", () => {
  beforeEach(() => {
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReset().mockReturnValue(true);
  });

  function chooseSqlServer() {
    renderDialog();
    fireEvent.change(field("Database type"), { target: { value: "mssql" } });
  }

  it("shows the SQL Server fields with port 1433 and the instance hint", () => {
    chooseSqlServer();
    expect(field("Port")).toHaveValue("1433");
    expect(field("Instance name (optional)")).toHaveAccessibleDescription(
      /port it listens on above/,
    );
    expect(field("Database (optional)")).toHaveAttribute("placeholder", "login default");
    expect(field("Database (optional)")).toHaveAccessibleDescription(/login's default database/);
    expect(screen.getByLabelText(/Read-only intent/)).not.toBeChecked();
  });

  it("offers strict encryption only with Require or a verifying mode", () => {
    chooseSqlServer();
    fireEvent.click(screen.getByRole("tab", { name: "SSL/TLS" }));
    const strict = screen.getByLabelText("TDS 8.0 strict encryption");
    expect(field("Encryption mode")).toHaveValue("prefer");
    expect(screen.getByTestId("tls-help")).toHaveTextContent(/TrustServerCertificate/);
    expect(strict).toBeDisabled();
    fireEvent.change(field("Encryption mode"), { target: { value: "verify_full" } });
    expect(strict).toBeEnabled();
    fireEvent.click(strict);
    expect(strict).toBeChecked();
    // Client certificates are not used by SQL Server connections.
    expect(screen.queryByLabelText("Client certificate (optional)")).toBeNull();
    expect(screen.getByTestId("no-client-cert")).toHaveTextContent("SQL Server");
  });

  it("refuses strict encryption with a weaker mode and jumps to it", async () => {
    chooseSqlServer();
    type("Connection name", "sql");
    fireEvent.click(screen.getByRole("tab", { name: "SSL/TLS" }));
    fireEvent.change(field("Encryption mode"), { target: { value: "require" } });
    fireEvent.click(screen.getByLabelText("TDS 8.0 strict encryption"));
    fireEvent.change(field("Encryption mode"), { target: { value: "prefer" } });
    fireEvent.click(screen.getByRole("tab", { name: "General" }));
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    const strict = screen.getByLabelText("TDS 8.0 strict encryption");
    await waitFor(() => expect(strict).toHaveFocus());
    expect(strict).toHaveAttribute("aria-invalid", "true");
    expect(strict).toHaveAccessibleDescription(/needs the encryption mode Require/);
    expect(tauriCore.invoke).not.toHaveBeenCalled();
  });

  it("asks for the port of a named instance", () => {
    chooseSqlServer();
    type("Connection name", "sql");
    type("Instance name (optional)", "SQLEXPRESS");
    type("Port", "");
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(field("Port")).toHaveAccessibleDescription(/named instance's port/);
    expect(tauriCore.invoke).not.toHaveBeenCalled();
  });

  it("saves the SQL Server options with the profile", async () => {
    const saved = { id: "0190f000-0000-7000-8000-000000000002" };
    mockCommands({ save_profile: () => saved });
    const { onSaved } = renderDialog();
    fireEvent.change(field("Database type"), { target: { value: "mssql" } });
    type("Connection name", "sql");
    type("Instance name (optional)", "SQLEXPRESS");
    type("Port", "14330");
    fireEvent.click(screen.getByLabelText(/Read-only intent/));
    fireEvent.click(screen.getByRole("tab", { name: "SSL/TLS" }));
    fireEvent.change(field("Encryption mode"), { target: { value: "verify_ca" } });
    fireEvent.click(screen.getByLabelText("TDS 8.0 strict encryption"));
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(onSaved).toHaveBeenCalledWith(saved));
    const [, args] = tauriCore.invoke.mock.calls[0] ?? [];
    const { profile } = args as { profile: ConnectionProfile };
    expect(profile.engine).toBe("mssql");
    expect(profile.address).toEqual({ kind: "network", host: "localhost", port: 14330 });
    expect(profile.tls.mode).toBe("verify_ca");
    expect(profile.options).toEqual({
      instance: "SQLEXPRESS",
      encrypt: "strict",
      application_intent: "read_only",
    });
  });
});
