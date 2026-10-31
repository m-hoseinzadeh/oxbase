import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { StatusBar } from "./StatusBar";
import { tauriCore } from "./test/tauriMock";

vi.mock("@tauri-apps/api/core", async () => (await import("./test/tauriMock")).tauriCore);

describe("StatusBar", () => {
  beforeEach(() => {
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReset().mockReturnValue(true);
  });

  it("shows the core version obtained over IPC", async () => {
    tauriCore.invoke.mockResolvedValue({ version: "0.0.1", platform: "linux", arch: "x86_64" });
    render(<StatusBar />);
    expect(await screen.findByText("oxbase core 0.0.1 · linux/x86_64")).toBeInTheDocument();
    expect(tauriCore.invoke).toHaveBeenCalledWith("app_info");
  });

  it("explains when the core is not available", async () => {
    tauriCore.isTauri.mockReturnValue(false);
    render(<StatusBar />);
    expect(await screen.findByText("Core unavailable (browser preview)")).toBeInTheDocument();
    expect(tauriCore.invoke).not.toHaveBeenCalled();
  });
});
