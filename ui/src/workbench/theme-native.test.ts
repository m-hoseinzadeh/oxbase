import { afterEach, describe, expect, it, vi } from "vitest";

const setTheme = vi.fn(async () => {});
vi.mock("@tauri-apps/api/core", () => ({ isTauri: () => true }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ setTheme }) }));

const { nativeTheme, setThemePreference } = await import("./theme-bridge");

describe("native window theme", () => {
  afterEach(() => {
    setThemePreference("dark");
    setTheme.mockClear();
  });

  it("maps the choice to the window theme (system follows the OS)", () => {
    expect(nativeTheme("dark")).toBe("dark");
    expect(nativeTheme("light")).toBe("light");
    expect(nativeTheme("system")).toBeNull();
  });

  it("passes every change of the choice to the native window", () => {
    setThemePreference("light");
    expect(setTheme).toHaveBeenLastCalledWith("light");
    setThemePreference("system");
    expect(setTheme).toHaveBeenLastCalledWith(null);
    setThemePreference("dark");
    expect(setTheme).toHaveBeenLastCalledWith("dark");
  });

  it("keeps working when the window refuses", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    setTheme.mockRejectedValueOnce(new Error("not allowed"));
    setThemePreference("light");
    await Promise.resolve();
    await Promise.resolve();
    expect(warn).toHaveBeenCalled();
    expect(document.documentElement.dataset.theme).toBe("light");
    warn.mockRestore();
  });
});
