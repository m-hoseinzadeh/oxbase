import { ENGINE_INFO, type Engine } from "./types";

/**
 * A small coloured tile with the engine's initials: the only place engine colours appear.
 * Decorative: the engine's name is always shown as text next to it.
 */
export function EngineBadge({ engine, size = "md" }: { engine: Engine; size?: "sm" | "md" }) {
  return (
    <span
      className={`engine-badge engine-badge--${engine}${size === "sm" ? " engine-badge--sm" : ""}`}
      aria-hidden="true"
    >
      {ENGINE_INFO[engine].short}
    </span>
  );
}
