/**
 * The value panel next to a result grid: the selected cell's whole value (./ValueView), read
 * only, following the selection.
 */
import { X } from "lucide-react";
import type { ColumnInfo, Engine } from "../../api";
import { ICON_SM } from "../../components/icons";
import { columnTypeLabel } from "./format";
import { ValueView } from "./ValueView";

export interface ValuePanelProps {
  /** The selected cell's column; `null` = no cell selected. */
  column: ColumnInfo | null;
  /** The value: text, `null` (SQL NULL) or `undefined` (no cell, or its row is loading). */
  value: string | null | undefined;
  /** The raw bytes of a binary value (an image shows as the image). */
  bytes?: Uint8Array | null | undefined;
  /** The connection's engine, for the SQL beautifier and grammar. */
  engine?: Engine | null | undefined;
  onClose: () => void;
}

export function ValuePanel({ column, value, bytes, engine, onClose }: ValuePanelProps) {
  return (
    <aside
      className="value-panel"
      aria-label="Value"
      data-testid="value-panel"
      onKeyDown={(event) => {
        if (event.key === "Escape") {
          event.stopPropagation();
          onClose();
        }
      }}
    >
      <header className="value-panel__header">
        <div className="value-panel__title">
          <span className="value-panel__name">{column ? column.name : "Value"}</span>
          {column ? <span className="value-panel__type">{columnTypeLabel(column)}</span> : null}
        </div>
        <button
          type="button"
          className="icon-button"
          aria-label="Close the value panel"
          title="Close (Esc)"
          onClick={onClose}
        >
          <X {...ICON_SM} />
        </button>
      </header>
      <ValueView
        value={value}
        bytes={bytes}
        engine={engine}
        empty={column ? "Loading…" : "Select a cell to see its whole value."}
        label={column ? `Value of ${column.name}` : "Value"}
        testId="value-panel-text"
      />
    </aside>
  );
}
