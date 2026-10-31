import { ENGINE_INFO, type Engine } from "./types";

/** A small coloured tile with the engine's initials. Decorative: the label is shown as text. */
export function EngineBadge({ engine }: { engine: Engine }) {
  return (
    <span className={`engine-badge engine-badge--${engine}`} aria-hidden="true">
      {ENGINE_INFO[engine].short}
    </span>
  );
}
