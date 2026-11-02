import { afterEach, describe, expect, it, vi } from "vitest";
import {
  applyMonacoTheme,
  normalizeColor,
  setThemePreference,
  subscribeTheme,
} from "./theme-bridge";

describe("normalizeColor", () => {
  it("expands the short hex forms a CSS minifier produces", () => {
    expect(normalizeColor("#fff")).toBe("#ffffff");
    expect(normalizeColor("#0F1A")).toBe("#00ff11aa");
    expect(normalizeColor("#0f172a")).toBe("#0f172a");
    expect(normalizeColor("#0284c733")).toBe("#0284c733");
  });

  it("converts rgb() in space and comma syntax, with alpha", () => {
    expect(normalizeColor("rgb(56 189 248 / 18%)")).toBe("#38bdf82e");
    expect(normalizeColor("rgba(2, 132, 199, 0.2)")).toBe("#0284c733");
    expect(normalizeColor("rgb(255, 255, 255)")).toBe("#ffffff");
  });

  it("rejects what Monaco could not use", () => {
    expect(normalizeColor("white")).toBeNull();
    expect(normalizeColor("var(--x)")).toBeNull();
  });
});

describe("applyMonacoTheme", () => {
  afterEach(() => setThemePreference("dark"));

  it("only ever gives Monaco long hex colours", () => {
    // Tokens as a minified stylesheet serves them.
    const style = document.documentElement.style;
    style.setProperty("--color-bg", "#fff");
    style.setProperty("--color-selection-strong", "rgb(2 132 199 / 20%)");
    const defineTheme = vi.fn();
    const setTheme = vi.fn();
    setThemePreference("light");
    applyMonacoTheme({ editor: { defineTheme, setTheme } });
    const [name, data] = defineTheme.mock.calls[0] as [string, { colors: Record<string, string> }];
    expect(name).toBe("oxbase-light");
    expect(setTheme).toHaveBeenCalledWith("oxbase-light");
    for (const value of Object.values(data.colors)) {
      expect(value).toMatch(/^#[0-9a-f]{6}([0-9a-f]{2})?$/);
    }
    expect(data.colors["editor.background"]).toBe("#ffffff");
    style.removeProperty("--color-bg");
    style.removeProperty("--color-selection-strong");
  });

  it("keeps notifying other surfaces when one listener throws", () => {
    const after = vi.fn();
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    const offA = subscribeTheme(() => {
      throw new Error("boom");
    });
    const offB = subscribeTheme(after);
    setThemePreference("light");
    expect(after).toHaveBeenCalled();
    offA();
    offB();
    error.mockRestore();
  });
});
