import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { App } from "./App";
import { tauriCore } from "./test/tauriMock";

vi.mock("@tauri-apps/api/core", async () => (await import("./test/tauriMock")).tauriCore);

describe("App layout", () => {
  beforeAll(() => {
    // jsdom has no ResizeObserver; dockview only needs the constructor to exist.
    globalThis.ResizeObserver ??= class {
      observe() {}
      unobserve() {}
      disconnect() {}
    };
  });

  beforeEach(() => {
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReset().mockReturnValue(false);
  });

  it("renders the Connections and Objects panels, the Welcome tab and the status bar", async () => {
    render(<App />);
    expect(await screen.findByText("No connections yet")).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Welcome to Oxbase" })).toBeInTheDocument();
    expect(screen.getByText("Connections")).toBeInTheDocument();
    expect(screen.getByText("Welcome")).toBeInTheDocument();
    expect(await screen.findByTestId("core-version")).toHaveTextContent("Core unavailable");
    // M2 workbench panels: the object tree and the query history.
    expect(screen.getByText("Objects")).toBeInTheDocument();
    expect(screen.getByText("No open connections")).toBeInTheDocument();
    const toolbar = screen.getByRole("toolbar", { name: "Workbench" });
    expect(within(toolbar).getByRole("button", { name: "New query" })).toBeEnabled();
  });

  it("closes a tab with a middle click, and only with the middle button", async () => {
    render(<App />);
    const tab = (await screen.findByText("Welcome")).closest(".dv-default-tab") as HTMLElement;
    expect(tab).not.toBeNull();
    fireEvent.pointerDown(tab, { button: 2 });
    fireEvent.pointerUp(tab, { button: 2 });
    expect(screen.getByRole("heading", { name: "Welcome to Oxbase" })).toBeInTheDocument();
    const middle = new MouseEvent("mousedown", { bubbles: true, cancelable: true, button: 1 });
    fireEvent(tab, middle);
    expect(middle.defaultPrevented).toBe(true); // no auto-scroll
    fireEvent.pointerDown(tab, { button: 1 });
    fireEvent.pointerUp(tab, { button: 1 });
    await waitFor(() =>
      expect(screen.queryByRole("heading", { name: "Welcome to Oxbase" })).toBeNull(),
    );
  });
});
