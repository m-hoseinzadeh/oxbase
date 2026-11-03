/** Small pieces shared by the M8 views: badges, notes, the progress bar, status icons. */
import {
  CircleAlert,
  CircleCheck,
  CircleMinus,
  CirclePlus,
  CircleSlash,
  Equal,
  Info,
  PenLine,
  TriangleAlert,
} from "lucide-react";
import type { ReactNode } from "react";
import type { DiffStatus, Fidelity } from "../../api";
import { ICON_SM } from "../../components/icons";
import { FIDELITY_HINT, FIDELITY_LABEL, fidelityTone, STATUS_LABEL } from "./model";

/** The lossiness of a type mapping: a word in a tinted badge, the reason as tooltip. */
export function FidelityBadge({
  fidelity,
  reason,
}: {
  fidelity: Fidelity;
  reason?: string | undefined;
}) {
  const tone = fidelityTone(fidelity);
  const Icon =
    fidelity === "exact"
      ? CircleCheck
      : fidelity === "widened"
        ? Info
        : fidelity === "narrowed"
          ? TriangleAlert
          : CircleSlash;
  const tip = reason ?? FIDELITY_HINT[fidelity];
  return (
    <span
      className={`badge badge--${tone} sync-badge`}
      title={tip}
      data-testid="fidelity-badge"
      data-fidelity={fidelity}
    >
      <Icon {...ICON_SM} />
      {FIDELITY_LABEL[fidelity]}
      <span className="visually-hidden">{`: ${tip}`}</span>
    </span>
  );
}

const STATUS_ICON: Record<DiffStatus, typeof CirclePlus> = {
  only_in_source: CirclePlus,
  only_in_target: CircleMinus,
  different: PenLine,
  same: Equal,
};

/** A diff item's status: icon plus a label (the icon alone never carries the meaning). */
export function StatusIcon({ status, label = false }: { status: DiffStatus; label?: boolean }) {
  const Icon = STATUS_ICON[status];
  return (
    <span className={`sync-status sync-status--${status}`} title={STATUS_LABEL[status]}>
      <Icon {...ICON_SM} />
      {label ? (
        <span className="sync-status__label">{STATUS_LABEL[status]}</span>
      ) : (
        <span className="visually-hidden">{STATUS_LABEL[status]}</span>
      )}
    </span>
  );
}

type Tone = "info" | "warning" | "error" | "ok";

/** A one-line note with an icon (`banner` of m3.css). */
export function Note({
  tone,
  title,
  children,
  testId,
  role,
}: {
  tone: Tone;
  title?: string;
  children?: ReactNode;
  testId?: string;
  role?: "status" | "alert" | "note";
}) {
  const Icon =
    tone === "ok"
      ? CircleCheck
      : tone === "error"
        ? CircleAlert
        : tone === "warning"
          ? TriangleAlert
          : Info;
  return (
    <div
      className={`banner banner--${tone} sync-note`}
      role={role ?? (tone === "error" ? "alert" : "note")}
      data-testid={testId}
    >
      <Icon {...ICON_SM} className="banner__icon" />
      <div className="banner__content">
        {title ? <p className="banner__title">{title}</p> : null}
        {children ? <div>{children}</div> : null}
      </div>
    </div>
  );
}

/** A list of warnings under a title, or nothing. */
export function WarningList({ title, items }: { title: string; items: string[] | undefined }) {
  if (!items || items.length === 0) return null;
  return (
    <Note tone="warning" title={title}>
      <ul className="banner__list">
        {items.map((w, i) => (
          // biome-ignore lint/suspicious/noArrayIndexKey: warnings can repeat.
          <li key={i}>{w}</li>
        ))}
      </ul>
    </Note>
  );
}

/** A determinate (or, without `total`, indeterminate) progress bar with its percentage. */
export function RunBar({
  done,
  total,
  label,
}: {
  done: number;
  total: number | null;
  label: string;
}) {
  const percent = total && total > 0 ? Math.min(100, Math.floor((done / total) * 100)) : null;
  return (
    <div className="sync-bar">
      {percent === null ? (
        <progress
          className="transfer-progress__bar transfer-progress__bar--indeterminate"
          aria-label={label}
        />
      ) : (
        <progress
          className="transfer-progress__bar"
          aria-label={label}
          max={100}
          value={percent}
          aria-valuetext={`${percent}%`}
        />
      )}
      <span className="sync-bar__percent" aria-hidden="true">
        {percent === null ? "" : `${percent}%`}
      </span>
    </div>
  );
}

/** "I understand …" checkbox of a destructive confirmation. */
export function Acknowledge({
  checked,
  onChange,
  children,
  testId,
}: {
  checked: boolean;
  onChange: (value: boolean) => void;
  children: ReactNode;
  testId?: string;
}) {
  return (
    <label className="checkbox sync-ack">
      <input
        type="checkbox"
        checked={checked}
        data-testid={testId}
        onChange={(e) => onChange(e.target.checked)}
      />
      {children}
    </label>
  );
}
