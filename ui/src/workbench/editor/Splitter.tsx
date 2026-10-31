/**
 * A horizontal splitter bar between two stacked panes: drag with the pointer, or focus it and
 * use the arrow keys (Home/End for the limits). `value` is the top pane's share (0..1).
 */
import { type KeyboardEvent, type PointerEvent, useRef } from "react";

const MIN = 0.1;
const MAX = 0.9;
const STEP = 0.05;

function clamp(v: number): number {
  return Math.min(MAX, Math.max(MIN, v));
}

export interface SplitterProps {
  value: number;
  onChange: (value: number) => void;
  label: string;
}

export function Splitter({ value, onChange, label }: SplitterProps) {
  const dragging = useRef(false);

  function onPointerDown(event: PointerEvent<HTMLDivElement>) {
    dragging.current = true;
    event.currentTarget.setPointerCapture?.(event.pointerId);
    event.preventDefault();
  }

  function onPointerMove(event: PointerEvent<HTMLDivElement>) {
    if (!dragging.current) return;
    const container = event.currentTarget.parentElement;
    if (!container) return;
    const box = container.getBoundingClientRect();
    if (box.height <= 0) return;
    onChange(clamp((event.clientY - box.top) / box.height));
  }

  function onPointerUp(event: PointerEvent<HTMLDivElement>) {
    dragging.current = false;
    event.currentTarget.releasePointerCapture?.(event.pointerId);
  }

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    let next: number | undefined;
    if (event.key === "ArrowUp") next = value - STEP;
    else if (event.key === "ArrowDown") next = value + STEP;
    else if (event.key === "Home") next = MIN;
    else if (event.key === "End") next = MAX;
    if (next === undefined) return;
    event.preventDefault();
    onChange(clamp(next));
  }

  return (
    // biome-ignore lint/a11y/useSemanticElements: a focusable, adjustable separator has no element.
    <div
      className="splitter"
      role="separator"
      aria-orientation="horizontal"
      aria-label={label}
      aria-valuemin={MIN * 100}
      aria-valuemax={MAX * 100}
      aria-valuenow={Math.round(value * 100)}
      tabIndex={0}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerUp}
      onKeyDown={onKeyDown}
    />
  );
}
