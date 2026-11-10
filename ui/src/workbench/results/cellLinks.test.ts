import { describe, expect, it } from "vitest";
import { hitsLinkIcon, LINK_ICON_GAP, LINK_ICON_SIZE, linkIconBox } from "./cellLinks";

describe("the link icon of foreign-key cells", () => {
  it("sits at the right of a value, at the left of a right-aligned one", () => {
    expect(linkIconBox(120, 28, false)).toEqual({
      x: 120 - LINK_ICON_GAP - LINK_ICON_SIZE,
      y: 7,
      size: LINK_ICON_SIZE,
    });
    expect(linkIconBox(120, 28, true).x).toBe(LINK_ICON_GAP);
    // A column narrower than the icon keeps it inside the cell.
    expect(linkIconBox(10, 28, false).x).toBe(0);
  });

  it("takes clicks on the icon only", () => {
    expect(hitsLinkIcon(107, 14, 120, 28, false)).toBe(true);
    expect(hitsLinkIcon(40, 14, 120, 28, false)).toBe(false);
    expect(hitsLinkIcon(12, 14, 120, 28, true)).toBe(true);
    expect(hitsLinkIcon(107, 14, 120, 28, true)).toBe(false);
  });
});
