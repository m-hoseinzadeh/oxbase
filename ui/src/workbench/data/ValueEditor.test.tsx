import { fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import type { CellInput, InputKind } from "../../api";
import { ValueEditor } from "./ValueEditor";
import { formatJson, inputProblem, nowText } from "./valueInput";

function Harness({
  kind,
  initial,
  onChange,
  nullable = true,
  allowDefault = false,
}: {
  kind: InputKind;
  initial: CellInput;
  onChange: (v: CellInput) => void;
  nullable?: boolean;
  allowDefault?: boolean;
}) {
  const [value, setValue] = useState(initial);
  return (
    <ValueEditor
      kind={kind}
      value={value}
      label="v"
      nullable={nullable}
      allowDefault={allowDefault}
      onChange={(v) => {
        setValue(v);
        onChange(v);
      }}
    />
  );
}

describe("ValueEditor", () => {
  it("toggles NULL apart from an empty string", () => {
    const onChange = vi.fn();
    render(
      <Harness kind={{ type: "text" }} initial={{ type: "text", text: "" }} onChange={onChange} />,
    );
    expect(screen.getByRole("textbox", { name: "v" })).toHaveValue("");
    fireEvent.click(screen.getByRole("checkbox", { name: "v is NULL" }));
    expect(onChange).toHaveBeenLastCalledWith({ type: "null" });
    expect(screen.queryByRole("textbox", { name: "v" })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /v: NULL/ }));
    expect(onChange).toHaveBeenLastCalledWith({ type: "text", text: "" });
  });

  it("offers DEFAULT for new rows and no NULL for NOT NULL columns", () => {
    const onChange = vi.fn();
    render(
      <Harness
        kind={{ type: "integer" }}
        initial={{ type: "default" }}
        nullable={false}
        allowDefault
        onChange={onChange}
      />,
    );
    expect(screen.queryByRole("checkbox", { name: "v is NULL" })).not.toBeInTheDocument();
    expect(screen.getByRole("checkbox", { name: "v uses the default" })).toBeChecked();
    fireEvent.click(screen.getByRole("checkbox", { name: "v uses the default" }));
    fireEvent.change(screen.getByRole("textbox", { name: "v" }), { target: { value: "1.5" } });
    expect(onChange).toHaveBeenLastCalledWith({ type: "text", text: "1.5" });
    expect(screen.getByRole("note")).toHaveTextContent("expected a whole number");
  });

  it("edits booleans, enums and JSON with their own controls", () => {
    const onChange = vi.fn();
    const { unmount } = render(
      <Harness
        kind={{ type: "boolean" }}
        initial={{ type: "text", text: "t" }}
        onChange={onChange}
      />,
    );
    const select = screen.getByRole("combobox", { name: "v" });
    expect(select).toHaveValue("true");
    fireEvent.change(select, { target: { value: "false" } });
    expect(onChange).toHaveBeenLastCalledWith({ type: "text", text: "false" });
    unmount();

    render(
      <Harness
        kind={{ type: "enum", labels: ["small", "large"] }}
        initial={{ type: "text", text: "small" }}
        onChange={onChange}
      />,
    );
    fireEvent.change(screen.getByRole("combobox", { name: "v" }), { target: { value: "large" } });
    expect(onChange).toHaveBeenLastCalledWith({ type: "text", text: "large" });
  });

  it("validates and formats JSON", () => {
    const onChange = vi.fn();
    render(
      <Harness
        kind={{ type: "json" }}
        initial={{ type: "text", text: '{"a":' }}
        onChange={onChange}
      />,
    );
    expect(screen.getByRole("note")).toHaveTextContent("not valid JSON");
    expect(screen.getByRole("button", { name: "Format" })).toBeDisabled();
    fireEvent.change(screen.getByRole("textbox", { name: "v" }), {
      target: { value: '{"a":[1]}' },
    });
    fireEvent.click(screen.getByRole("button", { name: "Format" }));
    expect(onChange).toHaveBeenLastCalledWith({ type: "text", text: '{\n  "a": [\n    1\n  ]\n}' });
  });

  it("hints at the grammar of each kind", () => {
    expect(inputProblem({ type: "date" }, "2026-10-31")).toBeNull();
    expect(inputProblem({ type: "date" }, "31/10/2026")).toMatch(/YYYY-MM-DD/);
    expect(inputProblem({ type: "timestamp" }, "2026-10-31T10:00:00.123456")).toBeNull();
    expect(inputProblem({ type: "timestampTz" }, "2026-10-31 10:00:00+02:00")).toBeNull();
    expect(inputProblem({ type: "time" }, "25")).toMatch(/HH:MM/);
    expect(inputProblem({ type: "float" }, "-Infinity")).toBeNull();
    expect(inputProblem({ type: "decimal" }, "12.50")).toBeNull();
    expect(inputProblem({ type: "bytes" }, "0xDEADbeef")).toBeNull();
    expect(inputProblem({ type: "bytes" }, "zz")).toMatch(/hex/);
    expect(inputProblem({ type: "uuid" }, "0192e2a0-0000-7000-8000-000000000001")).toBeNull();
    expect(inputProblem({ type: "other" }, "anything")).toBeNull();
    const at = new Date(2026, 9, 31, 8, 5, 9);
    expect(nowText({ type: "date" }, at)).toBe("2026-10-31");
    expect(nowText({ type: "timestamp" }, at)).toBe("2026-10-31 08:05:09");
    expect(nowText({ type: "text" }, at)).toBeNull();
    expect(formatJson("nope")).toBe("nope");
  });
});
