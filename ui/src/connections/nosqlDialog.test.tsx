import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { tauriCore } from "../test/tauriMock";
import { ConnectionDialog } from "./ConnectionDialog";
import type { ConnectionProfile } from "./types";

vi.mock("@tauri-apps/api/core", async () => (await import("../test/tauriMock")).tauriCore);
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: async () => null }));

const field = (name: RegExp | string) => screen.getByLabelText(name);
const type = (name: RegExp | string, value: string) =>
  fireEvent.change(field(name), { target: { value } });

function renderDialog(profile?: ConnectionProfile) {
  const onSaved = vi.fn();
  render(<ConnectionDialog profile={profile} onClose={() => {}} onSaved={onSaved} />);
  return { onSaved };
}

function savedProfile(): ConnectionProfile {
  const [, args] = tauriCore.invoke.mock.calls.find(([cmd]) => cmd === "save_profile") ?? [];
  return (args as { profile: ConnectionProfile }).profile;
}

describe("ConnectionDialog for MongoDB and Redis", () => {
  beforeEach(() => {
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReset().mockReturnValue(true);
    tauriCore.invoke.mockImplementation(async (cmd) => {
      if (cmd === "save_profile") return { id: "saved" };
      throw new Error(`unexpected command ${cmd}`);
    });
  });

  it("offers both engines", () => {
    renderDialog();
    expect(screen.getByRole("radio", { name: "MongoDB" })).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: "Redis" })).toBeInTheDocument();
  });

  it("saves MongoDB hosts, replica set, auth source, mechanism and read preference", async () => {
    const { onSaved } = renderDialog();
    fireEvent.click(screen.getByRole("radio", { name: "MongoDB" }));
    expect(field("Port")).toHaveValue("27017");
    type("Connection name", "mongo");
    type("More hosts (optional)", "db2:27018");
    type("Replica set (optional)", "rs0");
    type("Authentication database", "admin");
    fireEvent.change(field("Authentication mechanism"), { target: { value: "SCRAM-SHA-1" } });
    fireEvent.change(field("Read preference"), { target: { value: "nearest" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(onSaved).toHaveBeenCalled());
    expect(savedProfile()).toMatchObject({
      engine: "mongodb",
      address: { kind: "network", host: "localhost", port: 27017 },
      options: {
        hosts: "db2:27018",
        replica_set: "rs0",
        auth_source: "admin",
        auth_mechanism: "SCRAM-SHA-1",
        read_preference: "nearest",
      },
    });
  });

  it("disables the port for an SRV name and refuses SRV through SSH", async () => {
    renderDialog();
    fireEvent.click(screen.getByRole("radio", { name: "MongoDB" }));
    type("Connection name", "atlas");
    fireEvent.click(field(/SRV name/));
    expect(field("Port")).toBeDisabled();
    expect(screen.queryByLabelText("More hosts (optional)")).toBeNull();
    fireEvent.click(screen.getByRole("tab", { name: "SSH" }));
    fireEvent.click(screen.getByLabelText("Connect through an SSH tunnel"));
    type("SSH host", "bastion");
    type("SSH user name", "me");
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByText(/An SRV name stands alone/)).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: "General" })).toHaveAttribute("aria-selected", "true");
    expect(tauriCore.invoke).not.toHaveBeenCalled();
  });

  it("saves a Redis sentinel profile and asks for the master name", async () => {
    const { onSaved } = renderDialog();
    fireEvent.click(screen.getByRole("radio", { name: "Redis" }));
    expect(field("Port")).toHaveValue("6379");
    expect(field("Database index (optional)")).toBeInTheDocument();
    type("Connection name", "cache");
    fireEvent.change(field("Mode"), { target: { value: "sentinel" } });
    type("More sentinels", "s2:26379");
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByText(/name of the master/)).toBeInTheDocument();
    type("Master name", "oxmaster");
    fireEvent.change(field("Sentinel login"), { target: { value: "same" } });
    fireEvent.change(field("Protocol"), { target: { value: "resp3" } });
    type("Namespace separator", "/");
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(onSaved).toHaveBeenCalled());
    expect(savedProfile()).toMatchObject({
      engine: "redis",
      options: {
        mode: "sentinel",
        nodes: "s2:26379",
        master_name: "oxmaster",
        sentinel_auth: "same",
        protocol: "resp3",
        namespace_separator: "/",
      },
    });
  });

  it("edits a saved Redis cluster profile", () => {
    renderDialog({
      schema_version: 1,
      id: "p",
      name: "cluster",
      engine: "redis",
      address: { kind: "network", host: "127.0.0.1", port: 15501 },
      tls: { mode: "disable" },
      options: { mode: "cluster", nodes: "127.0.0.1:15502" },
    });
    expect(screen.getByRole("radio", { name: "Redis" })).toBeChecked();
    expect(field("Mode")).toHaveValue("cluster");
    expect(field("More cluster nodes")).toHaveValue("127.0.0.1:15502");
  });
});
