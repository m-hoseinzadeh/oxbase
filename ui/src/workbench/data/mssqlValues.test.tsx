import { fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import type { CellInput, DataType, InputKind } from "../../api";
import { ValueEditor } from "./ValueEditor";
import { byteLength, inputProblem, newUuid, typedProblem } from "./valueInput";

function Harness({
  kind,
  dataType,
  initial,
  onChange,
}: {
  kind: InputKind;
  dataType: DataType;
  initial: string;
  onChange: (v: CellInput) => void;
}) {
  const [value, setValue] = useState<CellInput>({ type: "text", text: initial });
  return (
    <ValueEditor
      kind={kind}
      dataType={dataType}
      value={value}
      label="v"
      nullable
      onChange={(v) => {
        setValue(v);
        onChange(v);
      }}
    />
  );
}

const MONEY: DataType = { type: "decimal", precision: 19, scale: 4 };

describe("SQL Server grid values", () => {
  it("uniqueidentifier: checks the text and makes new ones", () => {
    const onChange = vi.fn();
    render(
      <Harness
        kind={{ type: "uuid" }}
        dataType={{ type: "uuid" }}
        initial="6F9619FF-8B86-D011-B42D-00C04FC964FF"
        onChange={onChange}
      />,
    );
    expect(screen.queryByRole("note")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "New" }));
    const last = onChange.mock.lastCall?.[0] as CellInput | undefined;
    const text = last?.type === "text" ? last.text : "";
    expect(text).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    expect(newUuid()).not.toBe(newUuid());
    fireEvent.change(screen.getByRole("textbox", { name: "v" }), { target: { value: "nope" } });
    expect(screen.getByRole("note")).toHaveTextContent("a UUID");
  });

  it("datetimeoffset: 7 fraction digits and the stored offset", () => {
    const kind: InputKind = { type: "timestampTz" };
    expect(inputProblem(kind, "2026-01-15T10:20:30.1234567+03:30")).toBeNull();
    expect(inputProblem(kind, "2026-01-15 10:20:30-08:00")).toBeNull();
    expect(inputProblem(kind, "2026-01-15 10:20")).toBeNull();
    expect(inputProblem(kind, "15/01/2026")).toMatch(/HH:MM/);
  });

  it("money: at most 4 decimal places and 15 whole digits", () => {
    const kind: InputKind = { type: "decimal" };
    expect(typedProblem(kind, "922337203685477.5807", MONEY)).toBeNull();
    const small: DataType = { type: "decimal", precision: 10, scale: 4 };
    expect(typedProblem(kind, "-214748.3648", small)).toBeNull();
    expect(typedProblem(kind, "1.23450", MONEY)).toBeNull();
    expect(typedProblem(kind, "1.23456", MONEY)).toBe("at most 4 decimal places");
    expect(typedProblem(kind, "1234567890123456", MONEY)).toBe(
      "at most 15 digits before the point",
    );
    expect(typedProblem(kind, "12,5", MONEY)).toMatch(/number/);
    const onChange = vi.fn();
    render(<Harness kind={kind} dataType={MONEY} initial="1.00001" onChange={onChange} />);
    expect(screen.getByRole("note")).toHaveTextContent("at most 4 decimal places");
  });

  it("varbinary: hex or base64 within the column's length", () => {
    const kind: InputKind = { type: "bytes" };
    expect(byteLength("0xCAFEBABE")).toBe(4);
    expect(byteLength("0x")).toBe(0);
    expect(byteLength("0xABC")).toBeNull();
    expect(byteLength("base64:AQID")).toBe(3);
    expect(byteLength("base64:AQ==")).toBe(1);
    expect(byteLength("base64:AQ=")).toBeNull();
    const vb4: DataType = { type: "var_binary", length: 4 };
    expect(typedProblem(kind, "0xCAFEBABE", vb4)).toBeNull();
    expect(typedProblem(kind, "0xCAFEBABE00", vb4)).toBe("5 bytes: at most 4 fit");
    expect(typedProblem(kind, "0xABC", vb4)).toMatch(/even number/);
    expect(typedProblem(kind, "0xCAFEBABE00", { type: "var_binary", length: null })).toBeNull();
  });

  it("xml: a multi-line editor that checks the fragment is well-formed", () => {
    const xml: DataType = { type: "other", name: "xml" };
    const kind: InputKind = { type: "other" };
    expect(typedProblem(kind, '<a b="1"/>', xml)).toBeNull();
    expect(typedProblem(kind, "<a/><b>text</b>", xml)).toBeNull();
    expect(typedProblem(kind, "<a>", xml)).toBe("not well-formed XML");
    // A leading XML declaration is valid for SQL Server's xml type.
    expect(typedProblem(kind, '<?xml version="1.0" encoding="UTF-8"?><a/>', xml)).toBeNull();
    expect(typedProblem(kind, '  <?xml version="1.0"?>\n<a/><b/>', xml)).toBeNull();
    expect(typedProblem(kind, '<?xml version="1.0"?><a>', xml)).toBe("not well-formed XML");
    // Only at the start.
    expect(typedProblem(kind, '<a/><?xml version="1.0"?>', xml)).toBe("not well-formed XML");
    expect(typedProblem(kind, "<a>", { type: "other", name: "hierarchyid" })).toBeNull();
    const onChange = vi.fn();
    render(<Harness kind={kind} dataType={xml} initial="<a/>" onChange={onChange} />);
    const box = screen.getByRole("textbox", { name: "v" });
    expect(box.tagName).toBe("TEXTAREA");
    fireEvent.change(box, { target: { value: "<a><b></a>" } });
    expect(screen.getByRole("note")).toHaveTextContent("not well-formed XML");
    expect(onChange).toHaveBeenLastCalledWith({ type: "text", text: "<a><b></a>" });
  });
});
