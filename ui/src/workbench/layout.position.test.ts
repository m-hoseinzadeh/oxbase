import type { DockviewApi } from "dockview-react";
import { describe, expect, it } from "vitest";
import { mainAreaPosition } from "./layout";

type Api = Pick<DockviewApi, "panels" | "getPanel">;

function fakeApi(ids: string[]): Api {
  const panels = ids.map((id) => ({ id }));
  return {
    panels: panels as unknown as Api["panels"],
    getPanel: ((id: string) => panels.find((p) => p.id === id)) as Api["getPanel"],
  };
}

describe("mainAreaPosition", () => {
  it("puts a new tab with the main area's tabs", () => {
    expect(mainAreaPosition(fakeApi(["connections", "objects", "welcome", "history"]))).toEqual({
      referencePanel: "welcome",
      direction: "within",
    });
    expect(
      mainAreaPosition(fakeApi(["connections", "objects", "history", "data-editor-1"])),
    ).toEqual({ referencePanel: "data-editor-1", direction: "within" });
  });

  it("rebuilds the main area when every main tab is closed, never joining Connections", () => {
    expect(mainAreaPosition(fakeApi(["connections", "objects", "history"]))).toEqual({
      referencePanel: "history",
      direction: "above",
    });
    expect(mainAreaPosition(fakeApi(["connections", "objects"]))).toEqual({ direction: "right" });
  });
});
