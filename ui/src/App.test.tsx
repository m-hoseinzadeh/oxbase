import { render, screen } from "@testing-library/react";
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

  it("renders the Connections panel, the Welcome tab and the status bar", async () => {
    render(<App />);
    expect(await screen.findByText("No connections yet")).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Welcome to oxbase" })).toBeInTheDocument();
    expect(screen.getByText("Connections")).toBeInTheDocument();
    expect(screen.getByText("Welcome")).toBeInTheDocument();
    expect(await screen.findByTestId("core-version")).toHaveTextContent("Core unavailable");
  });
});
