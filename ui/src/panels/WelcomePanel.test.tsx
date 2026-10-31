import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { tauriCore } from "../test/tauriMock";
import { WelcomePanel } from "./WelcomePanel";

vi.mock("@tauri-apps/api/core", async () => (await import("../test/tauriMock")).tauriCore);

// Bytes produced by `app_tauri::demo_page::demo_page_bytes` (arrow-rs StreamWriter), so this
// test proves the JS decoder reads what the Rust core writes.
const here = dirname(fileURLToPath(import.meta.url));
const rustDemoPage = readFileSync(join(here, "../test/fixtures/demo-page.arrows"));

describe("WelcomePanel", () => {
  beforeEach(() => {
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReset().mockReturnValue(true);
  });

  it("decodes the Arrow IPC demo page and shows its row count", async () => {
    // Copy into a standalone ArrayBuffer, which is what Tauri hands to `invoke` callers.
    const buffer = new Uint8Array(rustDemoPage).buffer;
    tauriCore.invoke.mockResolvedValue(buffer);
    render(<WelcomePanel />);
    expect(await screen.findByTestId("demo-row-count")).toHaveTextContent("5");
    expect(screen.getByTestId("demo-page")).toHaveTextContent("5 rows × 3 columns");
    expect(tauriCore.invoke).toHaveBeenCalledWith("demo_arrow_page");
  });

  it("reports decode failures", async () => {
    tauriCore.invoke.mockResolvedValue(new Uint8Array([1, 2, 3]));
    render(<WelcomePanel />);
    expect(await screen.findByText(/Demo Arrow page failed/)).toBeInTheDocument();
  });
});
