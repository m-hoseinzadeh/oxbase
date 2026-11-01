/**
 * The progress of an import or export: a bar (when the total is known), counters, the rate and
 * the time left.
 */
import { formatMs } from "../format";
import type { Rates } from "./model";
import { formatBytes, formatCount } from "./model";

export interface ProgressFigure {
  label: string;
  value: string;
  /** Highlighted (errors). */
  alert?: boolean;
}

function Figure({ label, value, alert }: ProgressFigure) {
  return (
    <div className={`transfer-progress__figure${alert ? " transfer-progress__figure--alert" : ""}`}>
      <dt>{label}</dt>
      <dd data-testid={`progress-${label.toLowerCase().replace(/\s+/g, "-")}`}>{value}</dd>
    </div>
  );
}

export function ProgressView({
  label,
  rates,
  figures,
  unit,
}: {
  /** What is moving ("Importing orders.csv into public.orders"). */
  label: string;
  rates: Rates;
  figures: ProgressFigure[];
  /** What the rate counts besides rows ("read" / "written"). */
  unit: string;
}) {
  const percent = rates.fraction === null ? null : Math.floor(rates.fraction * 100);
  const rate: string[] = [];
  if (rates.rowsPerSecond !== null)
    rate.push(`${formatCount(Math.round(rates.rowsPerSecond))} rows/s`);
  if (rates.bytesPerSecond !== null)
    rate.push(`${formatBytes(Math.round(rates.bytesPerSecond))}/s ${unit}`);
  return (
    <div className="transfer-progress" data-testid="transfer-progress">
      <p className="transfer-progress__label">{label}</p>
      {percent === null ? (
        <progress className="transfer-progress__bar" aria-label="Progress" />
      ) : (
        <progress
          className="transfer-progress__bar"
          aria-label="Progress"
          max={100}
          value={percent}
          aria-valuetext={`${percent}%`}
        />
      )}
      <dl className="transfer-progress__figures">
        {percent !== null ? <Figure label="Done" value={`${percent}%`} /> : null}
        {figures.map((f) => (
          <Figure key={f.label} {...f} />
        ))}
        <Figure label="Rate" value={rate.length > 0 ? rate.join(" · ") : "—"} />
        <Figure
          label="Time left"
          value={rates.etaMs === null ? "estimating…" : `about ${formatMs(rates.etaMs)}`}
        />
      </dl>
    </div>
  );
}
