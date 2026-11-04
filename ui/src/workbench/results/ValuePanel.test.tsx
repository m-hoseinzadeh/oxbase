import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ColumnInfo } from "../../api";
import type { SqlEditorProps } from "../editor/SqlEditor";
import { ValuePanel } from "./ValuePanel";

// The editor is Monaco in the app; here a plain element shows what it would show.
vi.mock("../editor/SqlEditor", () => ({
  SqlEditor: ({ value, language, wordWrap, testId }: SqlEditorProps) => (
    <pre data-testid={testId} data-language={language} data-wrap={String(wordWrap)}>
      {value}
    </pre>
  ),
}));

const formatSql = vi.fn();
vi.mock("../../api", async (original) => ({
  ...(await original<typeof import("../../api")>()),
  formatSql: (...args: unknown[]) => formatSql(...args),
}));

const column: ColumnInfo = { name: "payload", dataType: "Utf8", nullable: true };
const shown = () => screen.getByTestId("value-panel-text");

describe("ValuePanel", () => {
  it("shows JSON beautified in Auto, and the value as it is in Text", () => {
    render(<ValuePanel column={column} value='{"a":1,"b":[true]}' onClose={() => {}} />);
    expect(shown().textContent).toBe('{\n  "a": 1,\n  "b": [\n    true\n  ]\n}');
    expect(shown().dataset.language).toBe("javascript");
    expect(screen.getByRole("combobox", { name: "Format" })).toHaveDisplayValue("Auto (JSON)");
    expect(screen.getByText(/18 characters · 1 line/)).not.toBeNull();

    fireEvent.change(screen.getByRole("combobox", { name: "Format" }), {
      target: { value: "text" },
    });
    expect(shown().textContent).toBe('{"a":1,"b":[true]}');
    expect(shown().dataset.language).toBe("plaintext");
  });

  it("says when the chosen beautifier cannot read the value", () => {
    render(<ValuePanel column={column} value="<a><b></a>" onClose={() => {}} />);
    fireEvent.change(screen.getByRole("combobox", { name: "Format" }), {
      target: { value: "xml" },
    });
    expect(screen.getByRole("status").textContent).toMatch(/Not well-formed XML/);
    expect(shown().textContent).toBe("<a><b></a>");
  });

  it("formats SQL with the core's formatter for the connection's engine", async () => {
    formatSql.mockResolvedValueOnce({
      range: { start: 0, end: 15 },
      text: "SELECT\n  1\nFROM\n  t",
    });
    render(
      <ValuePanel column={column} value="select 1 from t" engine="postgres" onClose={() => {}} />,
    );
    fireEvent.change(screen.getByRole("combobox", { name: "Format" }), {
      target: { value: "sql" },
    });
    await waitFor(() => expect(shown().textContent).toBe("SELECT\n  1\nFROM\n  t"));
    expect(formatSql).toHaveBeenCalledWith({
      engine: "postgres",
      text: "select 1 from t",
      range: null,
      options: null,
    });
    expect(shown().dataset.language).toBe("pgsql");
  });

  it("shows NULL, an empty selection and wraps by default; Escape closes", () => {
    const onClose = vi.fn();
    const { rerender } = render(<ValuePanel column={column} value={null} onClose={onClose} />);
    expect(screen.getByText("NULL")).not.toBeNull();
    rerender(<ValuePanel column={null} value={undefined} onClose={onClose} />);
    expect(screen.getByText(/Select a cell/)).not.toBeNull();
    rerender(<ValuePanel column={column} value="x" onClose={onClose} />);
    expect(shown().dataset.wrap).toBe("true");
    fireEvent.click(screen.getByRole("checkbox", { name: "Wrap" }));
    expect(shown().dataset.wrap).toBe("false");
    fireEvent.keyDown(screen.getByRole("complementary", { name: "Value" }), { key: "Escape" });
    expect(onClose).toHaveBeenCalled();
  });
});
