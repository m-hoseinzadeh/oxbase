import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { HistoryEntry, HistorySettings } from "../../api";
import { refreshAll, resetConnectionsStore } from "../../connections/store";
import type { ConnectionProfile, SessionSummary } from "../../connections/types";
import { tauriCore } from "../../test/tauriMock";
import { notifyHistoryChanged } from "../events";
import { HISTORY_PAGE, HistoryPanel, nextPageBefore, relativeTime } from "./HistoryPanel";

vi.mock("@tauri-apps/api/core", async () => (await import("../../test/tauriMock")).tauriCore);
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));
const layout = vi.hoisted(() => ({ openQueryEditor: vi.fn() }));
vi.mock("../layout", () => layout);

const PROFILE: ConnectionProfile = {
  schema_version: 1,
  id: "p-1",
  name: "Local PG",
  engine: "postgres",
  address: { kind: "network", host: "localhost" },
  tls: { mode: "disable" },
};

const SESSION: SessionSummary = {
  id: "s-1",
  profile_id: "p-1",
  profile_name: "Local PG",
  engine: "postgres",
  info: {
    server_version: { major: 18, minor: 0, patch: 0, build: 0, raw: "18.0" },
    tls_in_use: false,
    via_ssh: false,
    latency_ms: 1,
  },
  opened_at: "2026-10-31T10:00:00Z",
};

function entry(n: number, extra: Partial<HistoryEntry> = {}): HistoryEntry {
  return {
    id: `h-${n}`,
    profileId: "p-1",
    database: "app",
    sql: `SELECT ${n} FROM orders`,
    startedAt: new Date(Date.UTC(2026, 9, 31, 10, 0, 0) - n * 1000).toISOString(),
    durationMs: 12,
    status: "ok",
    rows: n,
    ...extra,
  };
}

let entries: HistoryEntry[] = [];
let settings: HistorySettings = { enabled: true, maxEntries: 10_000 };

function mockCore() {
  tauriCore.invoke.mockImplementation(async (cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "list_profiles":
        return [PROFILE];
      case "list_open_connections":
        return [SESSION];
      case "history_list": {
        const q = (a.query ?? {}) as { search?: string; before?: string; limit?: number };
        const matching = entries.filter(
          (e) =>
            (!q.search || e.sql.toLowerCase().includes(q.search.toLowerCase())) &&
            (!q.before || e.startedAt < q.before),
        );
        return matching.slice(0, q.limit ?? 200);
      }
      case "history_delete": {
        const ids = a.ids as string[];
        const before = entries.length;
        entries = entries.filter((e) => !ids.includes(e.id));
        return before - entries.length;
      }
      case "history_clear": {
        const n = entries.length;
        entries = [];
        return n;
      }
      case "history_get_settings":
        return settings;
      case "history_set_settings":
        settings = a.settings as HistorySettings;
        return null;
      default:
        throw new Error(`unexpected command ${cmd}`);
    }
  });
}

function historyCalls(): Array<Record<string, unknown>> {
  return tauriCore.invoke.mock.calls
    .filter(([cmd]) => cmd === "history_list")
    .map(([, args]) => (args as { query: Record<string, unknown> }).query);
}

/** The history entries shown (not the options of the connection filter). */
function shown(): HTMLElement[] {
  const list = screen.queryByRole("listbox", { name: "Query history" });
  return list ? within(list).queryAllByRole("option") : [];
}

async function renderPanel() {
  render(<HistoryPanel />);
  await act(async () => {
    await refreshAll();
  });
  return screen.findByRole("listbox", { name: "Query history" });
}

describe("HistoryPanel", () => {
  beforeEach(() => {
    resetConnectionsStore();
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReturnValue(true);
    layout.openQueryEditor.mockReset();
    entries = [
      entry(1),
      entry(2, { sql: "UPDATE users SET name = 'x'", status: "error", error: "permission denied" }),
      entry(3),
    ];
    settings = { enabled: true, maxEntries: 10_000 };
    mockCore();
  });
  afterEach(() => {
    resetConnectionsStore();
  });

  it("lists entries newest first with their details", async () => {
    const list = await renderPanel();
    const options = within(list).getAllByRole("option");
    expect(options).toHaveLength(3);
    expect(options[0]).toHaveTextContent("SELECT 1 FROM orders");
    expect(options[0]).toHaveTextContent("12 ms");
    expect(options[0]).toHaveTextContent("Local PG / app");
    expect(options[1]).toHaveTextContent("permission denied");
    expect(within(options[1] as HTMLElement).getByRole("img", { name: "Failed" })).toBeTruthy();
    expect(historyCalls()[0]).toEqual({ limit: HISTORY_PAGE });
  });

  it("offers reopen and delete on the hovered row without selecting it", async () => {
    const list = await renderPanel();
    const second = within(list).getAllByRole("option")[1] as HTMLElement;
    expect(within(second).queryByRole("button", { name: "Reopen in editor" })).toBeNull();
    fireEvent.mouseEnter(second);
    fireEvent.click(within(second).getByRole("button", { name: "Reopen in editor" }));
    expect(layout.openQueryEditor).toHaveBeenCalledWith(
      expect.objectContaining({ sql: "UPDATE users SET name = 'x'" }),
    );
    expect(second).toHaveAttribute("aria-selected", "false");
    fireEvent.click(within(second).getByRole("button", { name: "Delete from history" }));
    await waitFor(() => expect(shown()).toHaveLength(2));
    expect(tauriCore.invoke).toHaveBeenCalledWith("history_delete", { ids: ["h-2"] });
  });

  it("searches after typing settles", async () => {
    await renderPanel();
    fireEvent.change(screen.getByRole("searchbox", { name: "Search history" }), {
      target: { value: "update" },
    });
    await waitFor(() => expect(shown()).toHaveLength(1));
    expect(historyCalls().at(-1)).toEqual({ limit: HISTORY_PAGE, search: "update" });
    expect(shown()[0]).toHaveTextContent("UPDATE users");
  });

  it("reopens an entry in a query editor on its connection", async () => {
    const list = await renderPanel();
    const first = within(list).getAllByRole("option")[0] as HTMLElement;
    fireEvent.click(first);
    fireEvent.doubleClick(first);
    expect(layout.openQueryEditor).toHaveBeenCalledWith({
      sql: "SELECT 1 FROM orders",
      sessionId: "s-1",
      database: "app",
    });
    // Keyboard: focus the next entry and press Enter.
    fireEvent.keyDown(list, { key: "ArrowDown" });
    fireEvent.keyDown(list, { key: "Enter" });
    expect(layout.openQueryEditor).toHaveBeenLastCalledWith(
      expect.objectContaining({ sql: "UPDATE users SET name = 'x'" }),
    );
  });

  it("deletes the selected entries", async () => {
    const list = await renderPanel();
    const options = within(list).getAllByRole("option");
    fireEvent.click(options[0] as HTMLElement);
    fireEvent.click(options[2] as HTMLElement, { ctrlKey: true });
    fireEvent.click(screen.getByRole("button", { name: "Delete" }));
    await waitFor(() => expect(shown()).toHaveLength(1));
    expect(tauriCore.invoke).toHaveBeenCalledWith("history_delete", { ids: ["h-1", "h-3"] });
  });

  it("clears the history after confirming", async () => {
    await renderPanel();
    fireEvent.click(screen.getByRole("button", { name: "Clear…" }));
    const dialog = screen.getByRole("dialog", { name: "Clear history" });
    fireEvent.click(within(dialog).getByRole("button", { name: "Clear" }));
    expect(await screen.findByText("No history")).toBeInTheDocument();
    expect(tauriCore.invoke).toHaveBeenCalledWith("history_clear", { profileId: null });
  });

  it("turns recording off and on", async () => {
    await renderPanel();
    const toggle = await screen.findByRole("checkbox", { name: "Record history" });
    await waitFor(() => expect(toggle).toBeChecked());
    fireEvent.click(toggle);
    await waitFor(() => expect(toggle).not.toBeChecked());
    expect(tauriCore.invoke).toHaveBeenCalledWith("history_set_settings", {
      settings: { enabled: false, maxEntries: 10_000 },
    });
    expect(screen.getByText(/Recording is off/)).toBeInTheDocument();
  });

  it("does not skip entries that share the last entry's start time", async () => {
    // Statements of one script started in the same millisecond, across the page boundary.
    const same = new Date(Date.UTC(2026, 9, 31, 9, 0, 0)).toISOString();
    entries = [
      ...Array.from({ length: HISTORY_PAGE - 2 }, (_, i) => entry(i + 1)),
      ...Array.from({ length: 6 }, (_, i) => entry(1000 + i, { startedAt: same })),
      entry(5000),
    ];
    await renderPanel();
    expect(shown()).toHaveLength(HISTORY_PAGE);
    fireEvent.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(shown()).toHaveLength(HISTORY_PAGE + 5));
    expect(screen.queryByRole("button", { name: "Load more" })).not.toBeInTheDocument();
  });

  it("steps past a whole page of entries with one start time", async () => {
    const same = new Date(Date.UTC(2026, 9, 31, 9, 0, 0)).toISOString();
    entries = [
      ...Array.from({ length: HISTORY_PAGE + 3 }, (_, i) => entry(1000 + i, { startedAt: same })),
      entry(5000),
    ];
    await renderPanel();
    fireEvent.click(screen.getByRole("button", { name: "Load more" }));
    // The rest of that millisecond cannot be reached through the contract; older ones can.
    await waitFor(() => expect(shown()).toHaveLength(HISTORY_PAGE + 1));
  });

  it("reloads when a run ends and pages with Load more", async () => {
    entries = Array.from({ length: HISTORY_PAGE + 5 }, (_, i) => entry(i + 1));
    await renderPanel();
    expect(shown()).toHaveLength(HISTORY_PAGE);
    fireEvent.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(shown()).toHaveLength(HISTORY_PAGE + 5));
    const last = entries[HISTORY_PAGE - 1] as HistoryEntry;
    expect(historyCalls().at(-1)).toEqual({
      limit: HISTORY_PAGE,
      before: nextPageBefore(last.startedAt),
    });

    entries = [entry(0, { sql: "SELECT now()" })];
    act(() => notifyHistoryChanged());
    await waitFor(() => expect(shown()).toHaveLength(1));
  });
});

describe("relativeTime", () => {
  const now = Date.UTC(2026, 9, 31, 12, 0, 0);
  const ago = (ms: number) => new Date(now - ms).toISOString();

  it("says how long ago a run started", () => {
    expect(relativeTime(ago(10_000), now)).toBe("just now");
    expect(relativeTime(ago(5 * 60_000), now)).toBe("5 min ago");
    expect(relativeTime(ago(3 * 3_600_000), now)).toBe("3 h ago");
    expect(relativeTime(ago(26 * 3_600_000), now)).toBe("yesterday");
    expect(relativeTime(ago(4 * 86_400_000), now)).toBe("4 days ago");
  });

  it("shows the date of older entries and keeps text it cannot parse", () => {
    expect(relativeTime(ago(30 * 86_400_000), now)).toMatch(/^2026-10-0\d \d\d:\d\d$/);
    expect(relativeTime("not a date", now)).toBe("not a date");
  });
});
