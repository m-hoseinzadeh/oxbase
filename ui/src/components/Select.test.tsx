import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import { Select } from "./Select";

function Fruit({
  onPick = () => {},
  disabled = false,
}: {
  onPick?: (v: string) => void;
  disabled?: boolean;
}) {
  const [value, setValue] = useState("pear");
  return (
    <Select
      aria-label="Fruit"
      value={value}
      disabled={disabled}
      onChange={(e) => {
        setValue(e.target.value);
        onPick(e.target.value);
      }}
    >
      <option value="apple">Apple</option>
      <option value="pear">Pear</option>
      <option value="plum" disabled>
        Plum
      </option>
      <optgroup label="Citrus">
        <option value="lemon">Lemon</option>
        <option value="lime">Lime</option>
      </optgroup>
    </Select>
  );
}

const combo = () => screen.getByRole("combobox", { name: "Fruit" });
const list = () => screen.queryByRole("listbox");
/** An option of the themed list (the native <option>s have the same role). */
const item = (name: string) => within(screen.getByRole("listbox")).getByRole("option", { name });

describe("Select", () => {
  it("is still a native select: value, change events and role", () => {
    const onPick = vi.fn();
    render(<Fruit onPick={onPick} />);
    expect(combo()).toHaveValue("pear");
    expect(combo().tagName).toBe("SELECT");
    fireEvent.change(combo(), { target: { value: "apple" } });
    expect(onPick).toHaveBeenCalledWith("apple");
    expect(combo()).toHaveValue("apple");
  });

  it("opens the themed list instead of the native one and picks with the mouse", () => {
    const onPick = vi.fn();
    render(<Fruit onPick={onPick} />);
    const down = fireEvent.mouseDown(combo(), { button: 0 });
    expect(down).toBe(false); // the native popup is prevented
    expect(list()).not.toBeNull();
    expect(combo()).toHaveAttribute("aria-expanded", "true");
    const options = within(screen.getByRole("listbox")).getAllByRole("option");
    expect(options.map((o) => o.textContent)).toEqual(["Apple", "Pear", "Plum", "Lemon", "Lime"]);
    expect(item("Pear")).toHaveAttribute("aria-selected", "true");
    expect(item("Plum")).toHaveAttribute("aria-disabled", "true");
    expect(screen.getByText("Citrus")).not.toBeNull();

    fireEvent.click(item("Plum"));
    expect(onPick).not.toHaveBeenCalled();
    fireEvent.click(item("Lime"));
    expect(onPick).toHaveBeenCalledWith("lime");
    expect(combo()).toHaveValue("lime");
    expect(list()).toBeNull();
    expect(document.activeElement).toBe(combo());
  });

  it("works from the keyboard: open, move past disabled options, pick, Escape", () => {
    const onPick = vi.fn();
    render(<Fruit onPick={onPick} />);
    combo().focus();
    fireEvent.keyDown(combo(), { key: "Enter" });
    const box = screen.getByRole("listbox");
    expect(document.activeElement).toBe(box);
    const active = () => document.getElementById(box.getAttribute("aria-activedescendant") ?? "");
    expect(active()?.textContent).toBe("Pear");
    fireEvent.keyDown(box, { key: "ArrowDown" });
    expect(active()?.textContent).toBe("Lemon"); // Plum is disabled
    fireEvent.keyDown(box, { key: "End" });
    expect(active()?.textContent).toBe("Lime");
    fireEvent.keyDown(box, { key: "Home" });
    expect(active()?.textContent).toBe("Apple");
    fireEvent.keyDown(box, { key: "Enter" });
    expect(onPick).toHaveBeenLastCalledWith("apple");
    expect(list()).toBeNull();

    fireEvent.keyDown(combo(), { key: "ArrowDown", altKey: true });
    fireEvent.keyDown(screen.getByRole("listbox"), { key: "Escape" });
    expect(list()).toBeNull();
    expect(document.activeElement).toBe(combo());
    expect(onPick).toHaveBeenCalledTimes(1);
  });

  it("jumps by typing the start of a label", () => {
    render(<Fruit />);
    fireEvent.keyDown(combo(), { key: " " });
    const box = screen.getByRole("listbox");
    fireEvent.keyDown(box, { key: "l" });
    const active = () => document.getElementById(box.getAttribute("aria-activedescendant") ?? "");
    expect(active()?.textContent).toBe("Lemon");
    fireEvent.keyDown(box, { key: "i" });
    expect(active()?.textContent).toBe("Lime");
  });

  it("closes on a click elsewhere without changing the value", () => {
    const onPick = vi.fn();
    render(
      <div>
        <Fruit onPick={onPick} />
        <button type="button">elsewhere</button>
      </div>,
    );
    fireEvent.mouseDown(combo(), { button: 0 });
    expect(list()).not.toBeNull();
    act(() => {
      fireEvent.mouseDown(screen.getByRole("button", { name: "elsewhere" }));
    });
    expect(list()).toBeNull();
    expect(onPick).not.toHaveBeenCalled();
    expect(combo()).toHaveValue("pear");
  });

  it("does not open when disabled", () => {
    render(<Fruit disabled />);
    fireEvent.mouseDown(combo(), { button: 0 });
    fireEvent.keyDown(combo(), { key: "Enter" });
    expect(list()).toBeNull();
  });
});
