import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ContextMenu, type MenuItem, OPENS_SUBMENU } from "./ContextMenu";

function menu(onClose = vi.fn()) {
  const json = vi.fn();
  const view = vi.fn();
  const items: MenuItem[] = [
    { label: "View value", onSelect: view },
    {
      label: "Copy as",
      onSelect: OPENS_SUBMENU,
      submenu: [
        { label: "JSON", onSelect: json },
        { label: "Markdown table", onSelect: vi.fn() },
      ],
    },
  ];
  render(<ContextMenu x={10} y={10} label="Cell" items={items} onClose={onClose} />);
  return { json, view, onClose };
}

describe("ContextMenu", () => {
  it("opens a submenu with ArrowRight, closes it with ArrowLeft", () => {
    menu();
    const copyAs = screen.getByRole("menuitem", { name: "Copy as" });
    expect(screen.getByRole("menuitem", { name: "View value" })).toHaveFocus();
    fireEvent.keyDown(document.activeElement as Element, { key: "ArrowDown" });
    expect(copyAs).toHaveFocus();
    expect(copyAs).toHaveAttribute("aria-haspopup", "menu");
    fireEvent.keyDown(copyAs, { key: "ArrowRight" });
    expect(copyAs).toHaveAttribute("aria-expanded", "true");
    const sub = screen.getByRole("menu", { name: "Copy as" });
    expect(screen.getByRole("menuitem", { name: "JSON" })).toHaveFocus();
    fireEvent.keyDown(screen.getByRole("menuitem", { name: "JSON" }), { key: "ArrowLeft" });
    expect(sub).not.toBeInTheDocument();
    expect(screen.getByRole("menu", { name: "Cell" })).toBeInTheDocument();
  });

  it("runs a submenu item and closes every menu", () => {
    const { json, onClose } = menu();
    fireEvent.click(screen.getByRole("menuitem", { name: "Copy as" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "JSON" }));
    expect(json).toHaveBeenCalledTimes(1);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("keeps the menus open on a click inside the submenu, closes them outside", () => {
    const { onClose } = menu();
    fireEvent.mouseEnter(screen.getByRole("menuitem", { name: "Copy as" }));
    fireEvent.pointerDown(screen.getByRole("menuitem", { name: "Markdown table" }));
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.pointerDown(document.body);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("closes on a click in an element that cancels or stops the pointer event (the grid)", () => {
    const { onClose } = menu();
    const grid = document.createElement("canvas");
    document.body.append(grid);
    // Like Glide's cells: no `mousedown` follows, and the event does not bubble on.
    grid.addEventListener("pointerdown", (event) => {
      event.preventDefault();
      event.stopPropagation();
    });
    fireEvent.pointerDown(grid, { button: 0 });
    expect(onClose).toHaveBeenCalledTimes(1);
    grid.remove();
  });
});
