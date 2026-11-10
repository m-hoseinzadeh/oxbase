import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DocumentInfo, SavedQueryItem } from "../../api";
import type { ConnectionProfile } from "../../connections/types";
import { tauriCore } from "../../test/tauriMock";

vi.mock("@tauri-apps/api/core", async () => (await import("../../test/tauriMock")).tauriCore);

// The dockview layout is replaced: tabs are plain ids with titles.
const layout = vi.hoisted(() => ({
  titles: new Map<string, string>(),
  active: null as string | null,
  closed: [] as string[],
  opened: [] as Record<string, unknown>[],
}));
vi.mock("../layout", () => ({
  activatePanel: (id: string) => {
    layout.active = id;
  },
  activePanelId: () => layout.active,
  allPanelIds: () => [...layout.titles.keys()],
  closePanel: (id: string) => {
    layout.closed.push(id);
    layout.titles.delete(id);
  },
  dockviewApi: () => null,
  groupPanelIds: () => [...layout.titles.keys()],
  openQueryEditor: (init: Record<string, unknown>) => {
    layout.opened.push(init);
    return `editor-${layout.opened.length}`;
  },
  panelTitle: (id: string) => layout.titles.get(id) ?? "",
  setPanelTitle: (id: string, title: string) => {
    layout.titles.set(id, title);
  },
}));

import { getSettings, updateSettings } from "../../settings";
import { noteUnappliedChanges, resetUnappliedChanges } from "../unappliedChanges";
import {
  closeGroupPanels,
  closeTabs,
  saveTab,
  showOpenedDocument,
  suggestedFileName,
} from "./actions";
import { handleAppEvent } from "./appEvents";
import { DocumentDialogsHost, resetDocumentDialogs } from "./dialogs";
import { groupQueries } from "./QueriesPanel";
import {
  detachDocument,
  isDirty,
  markSaved,
  noteEditorReady,
  noteVersion,
  type QueryTabController,
  registerQueryTab,
  resetTabs,
  tabDocument,
} from "./tabs";
import { resetWorkspace, snapshotWorkspace, startWorkspace } from "./workspace";

const DOC: DocumentInfo = { id: "doc-1", name: "report.sql", location: "~/sql" };

/** A query tab: its text, version and connection are plain variables. */
function tab(
  id: string,
  title: string,
  options: { text?: string; profileId?: string; database?: string; document?: DocumentInfo } = {},
) {
  const state = { text: options.text ?? "", version: 1 };
  const controller: QueryTabController = {
    getText: () => state.text,
    getVersion: () => state.version,
    target: () => ({
      ...(options.profileId ? { profileId: options.profileId, profileName: "Local PG" } : {}),
      ...(options.database ? { database: options.database } : {}),
    }),
  };
  layout.titles.set(id, title);
  registerQueryTab(id, controller, options.document ? { document: options.document } : {});
  noteEditorReady(id, 1);
  return {
    edit(text: string) {
      state.text = text;
      state.version += 1;
      noteVersion(id, state.version);
    },
    undoTo(version: number, text: string) {
      state.text = text;
      state.version = version;
      noteVersion(id, version);
    },
  };
}

function answer(cmd: string, value: unknown) {
  tauriCore.invoke.mockImplementation(async (c: string) => {
    if (c === cmd) return value;
    throw { category: "unsupported", message: `unexpected ${c}` };
  });
}

beforeEach(() => {
  tauriCore.invoke.mockReset();
  tauriCore.isTauri.mockReset().mockReturnValue(true);
  layout.titles.clear();
  layout.active = null;
  layout.closed = [];
  layout.opened = [];
  resetTabs();
  resetDocumentDialogs();
  resetWorkspace();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("unsaved changes", () => {
  it("follow the editor's version, undo included", () => {
    const t = tab("editor-a", "report.sql", { text: "SELECT 1;", document: DOC });
    expect(isDirty("editor-a")).toBe(false);
    t.edit("SELECT 2;");
    expect(isDirty("editor-a")).toBe(true);
    t.undoTo(1, "SELECT 1;");
    expect(isDirty("editor-a")).toBe(false);
    t.edit("SELECT 3;");
    markSaved("editor-a", DOC);
    expect(isDirty("editor-a")).toBe(false);
  });

  it("stay until saved for restored text and deleted saved queries", () => {
    registerQueryTab(
      "editor-r",
      { getText: () => "x", getVersion: () => 1, target: () => ({}) },
      { document: DOC, dirty: true },
    );
    noteEditorReady("editor-r", 1);
    expect(isDirty("editor-r")).toBe(true);
    noteVersion("editor-r", 1);
    expect(isDirty("editor-r")).toBe(true);

    tab("editor-b", "report.sql", { document: DOC });
    expect(detachDocument(DOC.id)).toEqual(["editor-r", "editor-b"]);
    expect(tabDocument("editor-b")).toBeNull();
    expect(isDirty("editor-b")).toBe(true);
  });
});

describe("saving", () => {
  it("suggests file names", () => {
    expect(suggestedFileName("Query 3")).toBe("Query 3.sql");
    expect(suggestedFileName("a/b:c")).toBe("a_b_c.sql");
    expect(suggestedFileName("notes.txt")).toBe("notes.txt");
    expect(suggestedFileName("...")).toBe("query.sql");
  });

  it("writes a file tab to its file", async () => {
    const t = tab("editor-a", "report.sql", { text: "SELECT 1;", document: DOC });
    t.edit("SELECT 2;");
    answer("document_save", DOC);
    expect(await saveTab("editor-a")).toBe(true);
    expect(tauriCore.invoke).toHaveBeenCalledWith("document_save", {
      id: "doc-1",
      text: "SELECT 2;",
    });
    expect(isDirty("editor-a")).toBe(false);
  });

  it("asks a new tab on a connection for a saved query name", async () => {
    render(<DocumentDialogsHost />);
    tab("editor-q", "Query 1", { text: "SELECT 1;", profileId: "p-1", database: "sales" });
    const info: DocumentInfo = {
      id: "doc-q",
      name: "Totals",
      location: "Saved queries",
      savedQuery: { profileId: "p-1" },
    };
    answer("saved_query_create", info);
    let saved: Promise<boolean> = Promise.resolve(false);
    act(() => {
      saved = saveTab("editor-q");
    });
    const input = await screen.findByTestId("name-input");
    expect(input).toHaveValue("Query 1");
    fireEvent.change(input, { target: { value: "Totals" } });
    fireEvent.click(screen.getByTestId("name-submit"));
    expect(await saved).toBe(true);
    expect(tauriCore.invoke).toHaveBeenCalledWith("saved_query_create", {
      profileId: "p-1",
      name: "Totals",
      text: "SELECT 1;",
      database: "sales",
    });
    expect(layout.titles.get("editor-q")).toBe("Totals");
    expect(tabDocument("editor-q")).toEqual(info);
  });

  it("saves a new tab without a connection to a file", async () => {
    tab("editor-f", "Query 2", { text: "SELECT 2;" });
    const info: DocumentInfo = { id: "doc-f", name: "Query 2.sql", location: "~" };
    answer("document_save_as", info);
    expect(await saveTab("editor-f")).toBe(true);
    expect(tauriCore.invoke).toHaveBeenCalledWith("document_save_as", {
      from: null,
      defaultName: "Query 2.sql",
      text: "SELECT 2;",
      link: null,
    });
    expect(layout.titles.get("editor-f")).toBe("Query 2.sql");
  });
});

describe("closing", () => {
  it("closes clean tabs at once", async () => {
    tab("editor-a", "report.sql", { document: DOC });
    expect(await closeTabs(["editor-a", "welcome"])).toBe(true);
    expect(layout.closed).toEqual(["editor-a", "welcome"]);
  });

  it("asks about unsaved changes: cancel keeps the tabs, don't save closes them", async () => {
    render(<DocumentDialogsHost />);
    tab("editor-a", "report.sql", { document: DOC }).edit("changed");
    let closing: Promise<boolean> = Promise.resolve(false);
    act(() => {
      closing = closeTabs(["editor-a"]);
    });
    expect(await screen.findByText("report.sql")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(await closing).toBe(false);
    expect(layout.closed).toEqual([]);

    act(() => {
      closing = closeTabs(["editor-a"]);
    });
    fireEvent.click(await screen.findByTestId("unsaved-discard"));
    expect(await closing).toBe(true);
    expect(layout.closed).toEqual(["editor-a"]);
  });

  it("leaves the side panels open on close all", async () => {
    layout.titles.set("objects", "Objects");
    tab("editor-a", "Query 1");
    await closeGroupPanels("editor-a");
    expect(layout.closed).toEqual(["editor-a"]);
  });
});

describe("opening", () => {
  it("brings an open document forward instead of opening it twice", () => {
    tab("editor-a", "report.sql", { document: DOC });
    expect(showOpenedDocument({ document: DOC, text: "x" })).toBe("editor-a");
    expect(layout.active).toBe("editor-a");
    expect(layout.opened).toEqual([]);
  });

  it("opens an unlinked file without a connection and a linked one on its connection", () => {
    showOpenedDocument({ document: { ...DOC, id: "doc-2" }, text: "SELECT 1;" });
    showOpenedDocument({
      document: { ...DOC, id: "doc-3" },
      text: "SELECT 2;",
      link: { profileId: "p-1", database: "sales" },
    });
    expect(layout.opened[0]).toMatchObject({ sql: "SELECT 1;", pickConnection: true });
    expect(layout.opened[0]).not.toHaveProperty("profileId");
    expect(layout.opened[1]).toMatchObject({ profileId: "p-1", database: "sales" });
    expect(layout.opened[1]).not.toHaveProperty("pickConnection");
  });
});

describe("the workspace", () => {
  it("keeps the text of unsaved and untitled tabs, not of saved files", () => {
    tab("editor-a", "report.sql", { text: "on disk", document: DOC, profileId: "p-1" });
    tab("editor-b", "Query 2", { text: "SELECT 2;", database: "sales" });
    layout.active = "editor-b";
    expect(snapshotWorkspace()).toEqual({
      tabs: [
        { title: "report.sql", document: "doc-1", profileId: "p-1" },
        { title: "Query 2", text: "SELECT 2;", database: "sales" },
      ],
      active: 1,
    });
  });

  it("is saved a second after a change and once more when the app closes", async () => {
    vi.useFakeTimers();
    tauriCore.invoke.mockResolvedValue(undefined);
    startWorkspace();
    const t = tab("editor-a", "Query 1", { text: "a" });
    t.edit("ab");
    expect(tauriCore.invoke).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1000);
    expect(tauriCore.invoke).toHaveBeenCalledWith("workspace_save", {
      workspace: { tabs: [{ title: "Query 1", text: "ab" }] },
    });

    tauriCore.invoke.mockClear();
    t.edit("abc");
    updateSettings({ confirmQuit: false });
    handleAppEvent({ type: "closeRequested" });
    await vi.advanceTimersByTimeAsync(10);
    expect(tauriCore.invoke.mock.calls.map((c) => c[0])).toEqual([
      "workspace_save",
      "app_close_ready",
    ]);
    updateSettings({ confirmQuit: true });
  });
});

describe("quitting", () => {
  beforeEach(() => updateSettings({ confirmQuit: true }));
  afterEach(() => {
    updateSettings({ confirmQuit: true });
    resetUnappliedChanges();
  });

  const calls = () => tauriCore.invoke.mock.calls.map((c) => c[0]);

  it("asks first; Cancel keeps the app open and the next close asks again", async () => {
    tauriCore.invoke.mockResolvedValue(undefined);
    render(<DocumentDialogsHost />);
    act(() => handleAppEvent({ type: "closeRequested" }));
    await vi.waitFor(() => expect(calls()).toEqual(["app_close_wait"]));
    expect(screen.getByRole("dialog", { name: "Quit Oxbase?" })).toBeInTheDocument();
    // A second close while the question is open asks nothing more.
    act(() => handleAppEvent({ type: "closeRequested" }));
    expect(screen.getAllByRole("dialog")).toHaveLength(1);
    await act(async () => fireEvent.click(screen.getByTestId("quit-cancel")));
    await vi.waitFor(() => expect(calls()).toEqual(["app_close_wait", "app_close_cancel"]));
    expect(screen.queryByRole("dialog")).toBeNull();
    act(() => handleAppEvent({ type: "closeRequested" }));
    expect(screen.getByRole("dialog", { name: "Quit Oxbase?" })).toBeInTheDocument();
    await act(async () => fireEvent.click(screen.getByTestId("quit-cancel")));
  });

  it("names the tables whose pending changes would be lost, and quits on Quit", async () => {
    tauriCore.invoke.mockResolvedValue(undefined);
    noteUnappliedChanges("data-1", "public.orders", 3);
    noteUnappliedChanges("data-2", "public.items", 0);
    render(<DocumentDialogsHost />);
    act(() => handleAppEvent({ type: "closeRequested" }));
    const dialog = screen.getByRole("dialog", { name: "Quit Oxbase?" });
    expect(dialog).toHaveTextContent("public.orders (3 pending changes)");
    expect(dialog).not.toHaveTextContent("public.items");
    expect(screen.getByTestId("quit-confirm")).toHaveClass("button--danger");
    await act(async () => fireEvent.click(screen.getByTestId("quit-confirm")));
    await vi.waitFor(() => expect(calls()).toContain("app_close_ready"));
    expect(calls()[0]).toBe("app_close_wait");
    expect(calls()).not.toContain("app_close_cancel");
    expect(getSettings().confirmQuit).toBe(true);
  });

  it('"Don\'t ask again" turns the question off', async () => {
    tauriCore.invoke.mockResolvedValue(undefined);
    render(<DocumentDialogsHost />);
    act(() => handleAppEvent({ type: "closeRequested" }));
    fireEvent.click(screen.getByTestId("quit-dont-ask"));
    await act(async () => fireEvent.click(screen.getByTestId("quit-confirm")));
    expect(getSettings().confirmQuit).toBe(false);
    await vi.waitFor(() => expect(calls()).toContain("app_close_ready"));
  });
});

describe("the Queries panel", () => {
  it("groups saved queries by connection, unknown connections last", () => {
    const profile = (id: string, name: string) => ({ id, name }) as ConnectionProfile;
    const item = (id: string, profileId: string): SavedQueryItem => ({
      document: { id, name: id, location: "Saved queries", savedQuery: { profileId } },
      profileId,
      modifiedMs: 0,
    });
    const groups = groupQueries(
      [item("a", "p-2"), item("b", "gone"), item("c", "p-1"), item("d", "p-2")],
      [profile("p-1", "Alpha"), profile("p-2", "Beta"), profile("p-3", "Empty")],
    );
    expect(groups.map((g) => [g.profileId, g.queries.map((q) => q.document.id)])).toEqual([
      ["p-1", ["c"]],
      ["p-2", ["a", "d"]],
      ["gone", ["b"]],
    ]);
    expect(groups[2]?.profile).toBeNull();
  });
});
