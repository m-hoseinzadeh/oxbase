import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ColumnInfo } from "../../api";
import type { SqlEditorProps } from "../editor/SqlEditor";
import { ValueDialog } from "./ValueDialog";

// Monaco in the app; a textarea (editable) or a <pre> (read-only) here.
vi.mock("../editor/SqlEditor", () => ({
  SqlEditor: ({ value, initialValue, readOnly, onChange, testId, language }: SqlEditorProps) =>
    readOnly ? (
      <pre data-testid={testId} data-language={language}>
        {value}
      </pre>
    ) : (
      <textarea
        data-testid={testId}
        defaultValue={initialValue}
        onChange={(e) => onChange?.(e.target.value)}
      />
    ),
}));

const column: ColumnInfo = { name: "body", dataType: "Utf8", nullable: true };

describe("ValueDialog", () => {
  afterEach(() => vi.restoreAllMocks());

  it("shows the value; Edit loads the stored text and Save keeps the edit", async () => {
    const onSave = vi.fn();
    const onClose = vi.fn();
    render(
      <ValueDialog
        column={column}
        value="short…"
        edit={{
          initial: async () => ({ type: "text", text: "the whole text" }),
          nullable: true,
          onSave,
        }}
        onClose={onClose}
      />,
    );
    expect(screen.getByRole("dialog", { name: /body/ })).toBeInTheDocument();
    expect(screen.getByTestId("value-dialog-text").textContent).toBe("short…");
    fireEvent.click(screen.getByRole("button", { name: "Edit" }));
    const editor = await screen.findByTestId("value-dialog-editor");
    expect(editor).toHaveValue("the whole text");
    fireEvent.change(editor, { target: { value: "new text" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(onSave).toHaveBeenCalledWith({ type: "text", text: "new text" });
    expect(onClose).toHaveBeenCalled();
  });

  it("sets NULL, and offers no editing for a read-only cell", () => {
    const onSave = vi.fn();
    const { unmount } = render(
      <ValueDialog
        column={column}
        value="x"
        edit={{ initial: { type: "text", text: "x" }, nullable: true, onSave }}
        startEditing
        onClose={() => {}}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Set NULL" }));
    expect(onSave).toHaveBeenCalledWith({ type: "null" });
    unmount();
    render(<ValueDialog column={column} value="x" onClose={() => {}} />);
    expect(screen.queryByRole("button", { name: "Edit" })).toBeNull();
  });

  it("previews HTML in a sandboxed frame that loads nothing", async () => {
    render(<ValueDialog column={column} value="<p>Hello <b>you</b></p>" onClose={() => {}} />);
    expect(screen.getByRole("combobox", { name: "Format" })).toHaveDisplayValue("Auto (HTML)");
    expect(screen.getByTestId("value-dialog-text").dataset.language).toBe("html");
    fireEvent.click(screen.getByRole("checkbox", { name: "Preview" }));
    const frame = await screen.findByTestId("value-dialog-text-preview");
    expect(frame).toHaveAttribute("sandbox", "");
    expect(frame.getAttribute("srcdoc")).toMatch(
      /^<meta http-equiv="Content-Security-Policy" content="default-src 'none';/,
    );
    expect(frame.getAttribute("srcdoc")).toContain("<p>Hello <b>you</b></p>");
  });

  it("shows an image value as the image", async () => {
    const createObjectURL = vi.fn(() => "blob:image");
    const revokeObjectURL = vi.fn();
    Object.assign(URL, { createObjectURL, revokeObjectURL });
    const png = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 1]);
    render(
      <ValueDialog
        column={{ name: "photo", dataType: "Binary", nullable: true }}
        value="0x89504e47…"
        bytes={png}
        onClose={() => {}}
      />,
    );
    const image = await screen.findByRole("img", { name: /photo/ });
    expect(image).toHaveAttribute("src", "blob:image");
    expect(screen.getByRole("combobox", { name: "Format" })).toHaveDisplayValue("Auto (IMAGE)");
    // The hex text is still there as Text.
    fireEvent.change(screen.getByRole("combobox", { name: "Format" }), {
      target: { value: "text" },
    });
    await waitFor(() =>
      expect(screen.getByTestId("value-dialog-text").textContent).toBe("0x89504e47…"),
    );
  });
});
