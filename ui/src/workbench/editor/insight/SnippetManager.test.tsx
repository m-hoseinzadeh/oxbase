import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Snippet } from "../../../api";
import { tauriCore } from "../../../test/tauriMock";
import { draftInput, SnippetManager, validateDraft } from "./SnippetManager";

vi.mock("@tauri-apps/api/core", async () => (await import("../../../test/tauriMock")).tauriCore);

const BUILTIN: Snippet = {
  id: "builtin:select",
  name: "Select rows",
  prefix: "sel",
  body: "SELECT $\u007b1:*}\nFROM $\u007b2:table}$0",
  description: "A SELECT statement.",
  engines: [],
  builtin: true,
};
const PG_ONLY: Snippet = {
  id: "builtin:do-block",
  name: "DO block",
  prefix: "do",
  body: "DO $$\nBEGIN\n  $0\nEND\n$$;",
  description: "",
  engines: ["postgres"],
  builtin: true,
};
const MINE: Snippet = {
  id: "0190f5d2-6a8b-7c3e-9f00-0000000000aa",
  name: "Active users",
  prefix: "au",
  body: "SELECT * FROM users WHERE active$0",
  description: "",
  engines: ["mysql"],
  builtin: false,
};

let store: Snippet[];

function calls(cmd: string) {
  return tauriCore.invoke.mock.calls.filter(([c]) => c === cmd).map(([, a]) => a);
}

beforeEach(() => {
  store = [MINE, BUILTIN, PG_ONLY];
  tauriCore.invoke.mockReset();
  tauriCore.isTauri.mockReturnValue(true);
  tauriCore.invoke.mockImplementation(async (cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "snippet_list":
        return store;
      case "snippet_save": {
        const input = a.snippet as Record<string, unknown>;
        if (input.body === "SELECT ${") throw { category: "invalid_input", message: "bad body" };
        const saved: Snippet = {
          id: (input.id as string | undefined) ?? "0190f5d2-6a8b-7c3e-9f00-0000000000bb",
          name: input.name as string,
          prefix: input.prefix as string,
          body: input.body as string,
          description: (input.description as string) ?? "",
          engines: (input.engines as Snippet["engines"]) ?? [],
          builtin: false,
        };
        store = [saved, ...store.filter((s) => s.id !== saved.id)];
        return saved;
      }
      case "snippet_delete":
        store = store.filter((s) => s.id !== a.id);
        return true;
      default:
        throw new Error(`unexpected command ${cmd}`);
    }
  });
});

function field(name: string) {
  return screen.getByLabelText(name) as HTMLInputElement | HTMLTextAreaElement;
}

describe("snippet manager", () => {
  it("lists user snippets and the built-in ones of the engine", async () => {
    render(<SnippetManager engine="mysql" onClose={() => {}} />);
    const mine = await screen.findByRole("list", { name: "My snippets" });
    expect(within(mine).getByText("Active users")).toBeInTheDocument();
    const builtin = screen.getByRole("list", { name: "Built-in snippets" });
    expect(within(builtin).getByText("Select rows")).toBeInTheDocument();
    // PostgreSQL-only snippets are not offered for MySQL.
    expect(within(builtin).queryByText("DO block")).toBeNull();
    expect(calls("snippet_list")).toEqual([{ engine: null }]);
  });

  it("shows built-in snippets read-only and inserts them", async () => {
    const onInsert = vi.fn();
    const onClose = vi.fn();
    render(<SnippetManager engine="postgres" onClose={onClose} onInsert={onInsert} />);
    fireEvent.click(await screen.findByText("Select rows"));
    expect(screen.getByText(/FROM \$\{2:table\}/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Delete" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Insert" }));
    expect(onInsert).toHaveBeenCalledWith(BUILTIN.body);
    expect(onClose).toHaveBeenCalled();
  });

  it("adds a snippet after checking the form", async () => {
    render(<SnippetManager engine="postgres" onClose={() => {}} />);
    await screen.findByText("Select rows");
    fireEvent.click(screen.getByRole("button", { name: "New snippet" }));
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByText("Enter a name.")).toBeInTheDocument();
    expect(calls("snippet_save")).toHaveLength(0);

    fireEvent.change(field("Name"), { target: { value: "Recent orders" } });
    fireEvent.change(field("Prefix"), { target: { value: "bad prefix" } });
    fireEvent.change(field("Body"), { target: { value: "SELECT * FROM orders$0" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByText("Letters, digits, _ and - only.")).toBeInTheDocument();

    fireEvent.change(field("Prefix"), { target: { value: "ro" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls("snippet_save")).toHaveLength(1));
    expect(calls("snippet_save")[0]).toEqual({
      snippet: {
        name: "Recent orders",
        prefix: "ro",
        body: "SELECT * FROM orders$0",
        description: "",
        engines: ["postgres"],
      },
    });
    const mine = await screen.findByRole("list", { name: "My snippets" });
    await waitFor(() => expect(within(mine).getByText("Recent orders")).toBeInTheDocument());
  });

  it("edits, reports core errors and deletes after a confirmation", async () => {
    render(<SnippetManager engine="mysql" onClose={() => {}} />);
    fireEvent.click(await screen.findByText("Active users"));
    expect(field("Name").value).toBe("Active users");
    fireEvent.change(field("Body"), { target: { value: "SELECT ${" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("bad body");

    fireEvent.change(field("Body"), { target: { value: "SELECT 1$0" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls("snippet_save")).toHaveLength(2));
    expect((calls("snippet_save")[1] as { snippet: { id: string } }).snippet.id).toBe(MINE.id);

    fireEvent.click(screen.getByRole("button", { name: "Delete" }));
    expect(calls("snippet_delete")).toHaveLength(0);
    fireEvent.click(screen.getByRole("button", { name: "Confirm delete" }));
    await waitFor(() => expect(calls("snippet_delete")).toEqual([{ id: MINE.id }]));
    await waitFor(() => expect(screen.queryByText("Active users")).toBeNull());
  });
});

describe("snippet drafts", () => {
  it("validates like the core and builds the save payload", () => {
    const draft = {
      id: null,
      name: " x ",
      prefix: "a".repeat(41),
      description: "d".repeat(501),
      body: " ",
      engines: [],
    };
    expect(validateDraft(draft)).toEqual({
      prefix: "At most 40 characters.",
      description: "At most 500 characters.",
      body: "Enter the SQL to insert.",
    });
    expect(draftInput({ ...draft, id: "u-1", prefix: " p ", body: "SELECT 1" })).toEqual({
      id: "u-1",
      name: "x",
      prefix: "p",
      body: "SELECT 1",
      description: "d".repeat(501),
      engines: [],
    });
  });
});
