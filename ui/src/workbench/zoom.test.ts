import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

const setZoomNative = vi.fn(async (_level: number) => {});
vi.mock("@tauri-apps/api/core", () => ({ isTauri: () => true }));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({ setZoom: setZoomNative }),
}));

const { getZoom, resetZoom, setZoom, useZoom, zoomIn, zoomLabel, zoomOut, zoomStep } = await import(
  "./zoom"
);

describe("app zoom", () => {
  afterEach(() => {
    resetZoom();
    setZoomNative.mockClear();
  });

  it("steps through the levels and stops at the ends", () => {
    expect(zoomStep(1, 1)).toBe(1.1);
    expect(zoomStep(1, -1)).toBe(0.9);
    expect(zoomStep(2, 1)).toBe(2);
    expect(zoomStep(0.5, -1)).toBe(0.5);
    // A level between steps goes to the nearest step in that direction.
    expect(zoomStep(1.05, 1)).toBe(1.1);
    expect(zoomStep(1.05, -1)).toBe(1);
  });

  it("zooms the webview, remembers the level and resets to 100%", () => {
    zoomIn();
    zoomIn();
    expect(getZoom()).toBe(1.25);
    expect(setZoomNative).toHaveBeenLastCalledWith(1.25);
    expect(localStorage.getItem("oxbase.zoom")).toBe("1.25");
    zoomOut();
    expect(getZoom()).toBe(1.1);
    resetZoom();
    expect(setZoomNative).toHaveBeenLastCalledWith(1);
    expect(zoomLabel(1.1)).toBe("110%");
  });

  it("re-renders who shows the level", () => {
    const { result } = renderHook(() => useZoom());
    expect(result.current).toBe(1);
    act(() => setZoom(0.9));
    expect(result.current).toBe(0.9);
  });
});
