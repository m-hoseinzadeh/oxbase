/**
 * 1.5: the link icon of foreign-key cells (table data, "Open related row"). ResultGrid draws it
 * on the canvas and opens the related row when the icon is clicked. The icon sits at the right
 * of a cell, or at its left when the value is right-aligned (numbers), so it never covers the
 * start of a value.
 */

/** Icon size and the gap to the cell's edge, in CSS pixels. */
export const LINK_ICON_SIZE = 14;
export const LINK_ICON_GAP = 6;
/** Extra pixels around the icon that still count as a click on it. */
const HIT_SLACK = 3;

/** Lucide's "link" icon (24×24 viewBox), drawn with a 2px stroke. */
export const LINK_ICON_PATHS = [
  "M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71",
  "M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71",
];

export interface IconBox {
  x: number;
  y: number;
  size: number;
}

/** Where the icon goes in a `width` × `height` cell, relative to the cell's top-left corner. */
export function linkIconBox(width: number, height: number, alignRight: boolean): IconBox {
  const size = LINK_ICON_SIZE;
  const x = alignRight ? LINK_ICON_GAP : width - LINK_ICON_GAP - size;
  return { x: Math.max(0, x), y: Math.max(0, (height - size) / 2), size };
}

/** Whether a click at (`x`, `y`), relative to the cell, hits the icon. */
export function hitsLinkIcon(
  x: number,
  y: number,
  width: number,
  height: number,
  alignRight: boolean,
): boolean {
  const box = linkIconBox(width, height, alignRight);
  return (
    x >= box.x - HIT_SLACK &&
    x <= box.x + box.size + HIT_SLACK &&
    y >= box.y - HIT_SLACK &&
    y <= box.y + box.size + HIT_SLACK
  );
}

/** Draws the icon into a cell at `rect` (canvas coordinates), over a patch of `background`. */
export function drawLinkIcon(
  ctx: CanvasRenderingContext2D,
  rect: { x: number; y: number; width: number; height: number },
  alignRight: boolean,
  colour: string,
  background: string,
): void {
  // Path2D is missing in test DOMs; the grid then draws no icon.
  if (typeof Path2D === "undefined") return;
  const box = linkIconBox(rect.width, rect.height, alignRight);
  ctx.save();
  // Text that runs under the icon is covered, so the icon stays readable.
  ctx.fillStyle = background;
  ctx.fillRect(rect.x + box.x - 3, rect.y + 1, box.size + 6, rect.height - 2);
  ctx.translate(rect.x + box.x, rect.y + box.y);
  ctx.scale(box.size / 24, box.size / 24);
  ctx.strokeStyle = colour;
  ctx.lineWidth = 2;
  ctx.lineCap = "round";
  ctx.lineJoin = "round";
  for (const d of LINK_ICON_PATHS) ctx.stroke(new Path2D(d));
  ctx.restore();
}
