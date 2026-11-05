import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { tauriCore } from "../test/tauriMock";
import { SessionCounter } from "./SessionCounter";
import { refreshSessions } from "./store";
import type { SessionSummary, SessionUsage } from "./types";

vi.mock("@tauri-apps/api/core", async () => (await import("../test/tauriMock")).tauriCore);

const SESSION: SessionSummary = {
  id: "s-1",
  profile_id: "p-1",
  profile_name: "prod",
  engine: "postgres",
  info: {
    server_version: { major: 18, minor: 6, patch: 0, build: 0, raw: "18.6" },
    tls_in_use: false,
    via_ssh: false,
    latency_ms: 3,
  },
  opened_at: "2026-11-05T00:00:00Z",
  session_mode: "per_run",
};

const USAGE: SessionUsage[] = [
  {
    sessionId: "s-1",
    profileName: "prod",
    sessionMode: "per_run",
    total: 4,
    idleTabs: 0,
    running: 2,
    other: 1,
  },
  {
    sessionId: "s-2",
    profileName: "reporting",
    sessionMode: "per_tab",
    total: 3,
    idleTabs: 2,
    running: 0,
    other: 0,
  },
];

function answer(sessions: SessionSummary[], usage: SessionUsage[]) {
  tauriCore.invoke.mockImplementation(async (cmd: string) => {
    if (cmd === "list_open_connections") return sessions;
    if (cmd === "session_usage") return usage;
    throw new Error(`unexpected command ${cmd}`);
  });
}

describe("SessionCounter", () => {
  beforeEach(() => {
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReset().mockReturnValue(true);
  });
  afterEach(async () => {
    answer([], []);
    await act(() => refreshSessions());
  });

  it("is hidden without open connections", async () => {
    answer([], []);
    await act(() => refreshSessions());
    render(<SessionCounter />);
    expect(screen.queryByTestId("session-counter")).toBeNull();
  });

  it("counts every session and breaks them down per connection on click", async () => {
    answer([SESSION, { ...SESSION, id: "s-2", profile_name: "reporting" }], USAGE);
    await act(() => refreshSessions());
    render(<SessionCounter />);
    const counter = await screen.findByRole("button", { name: /7 sessions/ });
    expect(counter).toHaveAttribute("aria-expanded", "false");

    fireEvent.click(counter);
    expect(counter).toHaveAttribute("aria-expanded", "true");
    const popover = screen.getByRole("dialog", { name: "Database sessions" });
    const [prod, reporting] = within(popover).getAllByRole("listitem");
    expect(prod).toHaveTextContent("prod");
    expect(prod).toHaveTextContent("Connects for each run");
    expect(prod).toHaveTextContent("Running queries2");
    expect(prod).toHaveTextContent("Other (tree databases, edits, imports)1");
    expect(prod).not.toHaveTextContent("idle");
    expect(reporting).toHaveTextContent("Session per tab");
    expect(reporting).toHaveTextContent("Query tabs, idle2");

    // Escape closes it and gives the focus back.
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("dialog", { name: "Database sessions" })).toBeNull();
    expect(counter).toHaveFocus();
  });
});
