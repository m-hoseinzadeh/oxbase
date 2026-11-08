import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { getSettings, openSettings, updateSettings } from "../settings";
import { SettingsHost } from "./SettingsDialog";
import { getThemePreference, setThemePreference } from "./theme-bridge";
import { getZoom, resetZoom } from "./zoom";

describe("Settings window", () => {
  afterEach(() => {
    updateSettings({ gridDensity: "default", filterOpen: false, sessionMode: "per_tab" });
    setThemePreference("dark");
    resetZoom();
    localStorage.clear();
  });

  it("opens on request and applies every change at once", () => {
    render(<SettingsHost />);
    expect(screen.queryByRole("dialog", { name: "Settings" })).toBeNull();
    act(() => openSettings());
    const dialog = screen.getByRole("dialog", { name: "Settings" });

    fireEvent.change(within(dialog).getByLabelText("Theme"), { target: { value: "light" } });
    expect(getThemePreference()).toBe("light");
    fireEvent.change(within(dialog).getByLabelText("Zoom"), { target: { value: "1.25" } });
    expect(getZoom()).toBe(1.25);
    fireEvent.change(within(dialog).getByLabelText("Grid rows"), {
      target: { value: "compact" },
    });
    expect(getSettings().gridDensity).toBe("compact");
    fireEvent.change(within(dialog).getByLabelText("Rows to load when a table opens"), {
      target: { value: "5000" },
    });
    expect(localStorage.getItem("oxbase.tableData.rowLimit")).toBe("5000");
    fireEvent.click(within(dialog).getByRole("checkbox", { name: /Show Filter & sort/ }));
    expect(getSettings().filterOpen).toBe(true);
    fireEvent.change(within(dialog).getByLabelText("Sessions"), { target: { value: "per_run" } });
    expect(getSettings().sessionMode).toBe("per_run");
    // Remembered.
    expect(JSON.parse(localStorage.getItem("oxbase.settings") ?? "{}")).toMatchObject({
      gridDensity: "compact",
      filterOpen: true,
      sessionMode: "per_run",
    });

    expect(within(dialog).getByRole("table", { name: "Table data" })).toHaveTextContent(
      "Ctrl+SApply the pending changes",
    );
    fireEvent.click(within(dialog).getByRole("button", { name: "Close" }));
    expect(screen.queryByRole("dialog", { name: "Settings" })).toBeNull();
  });
});
