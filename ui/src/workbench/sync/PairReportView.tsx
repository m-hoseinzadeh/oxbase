/**
 * How every representative type maps from the source engine to the target engine
 * (`transfer_pair_report`), shown on demand in the transfer wizard's target step.
 */
import { LoaderCircle } from "lucide-react";
import { useEffect, useState } from "react";
import { type PairReport, transferPairReport } from "../../api";
import { ICON_SM } from "../../components/icons";
import { toCommandError } from "../../connections/api";
import { ENGINE_INFO } from "../../connections/types";
import { typeLabel } from "../transfer/model";
import { FidelityBadge, Note } from "./parts";

type State =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "ok"; report: PairReport }
  | { status: "error"; message: string };

export function PairReportView({
  sourceSession,
  targetSession,
}: {
  sourceSession: string;
  targetSession: string;
}) {
  const [open, setOpen] = useState(false);
  const [state, setState] = useState<State>({ status: "idle" });

  useEffect(() => {
    if (!open) return;
    let current = true;
    setState({ status: "loading" });
    transferPairReport(sourceSession, targetSession).then(
      (report) => current && setState({ status: "ok", report }),
      (error) => current && setState({ status: "error", message: toCommandError(error).message }),
    );
    return () => {
      current = false;
    };
  }, [open, sourceSession, targetSession]);

  const report = state.status === "ok" ? state.report : null;
  return (
    <details
      className="sync-pair-report"
      data-testid="transfer-pair-report"
      onToggle={(e) => setOpen((e.currentTarget as HTMLDetailsElement).open)}
    >
      <summary>
        {report
          ? `How types map from ${ENGINE_INFO[report.source].label} to ${ENGINE_INFO[report.target].label}`
          : "How types map between these engines"}
      </summary>
      {state.status === "loading" ? (
        <p className="transfer-step__status" role="status">
          <LoaderCircle {...ICON_SM} className="spin" />
          Loading…
        </p>
      ) : null}
      {state.status === "error" ? <Note tone="error">{state.message}</Note> : null}
      {report ? (
        <table className="transfer-table" aria-label="Type mapping">
          <thead>
            <tr>
              <th scope="col">Type</th>
              <th scope="col">Becomes</th>
              <th scope="col">Fidelity</th>
            </tr>
          </thead>
          <tbody>
            {report.rows.map((row) => (
              <tr key={JSON.stringify(row.logical)}>
                <th scope="row" className="transfer-table__type">
                  {typeLabel(row.logical)}
                </th>
                <td className="transfer-table__mono">
                  {row.mapping.nativeType || typeLabel(row.mapping.dataType)}
                </td>
                <td>
                  <FidelityBadge fidelity={row.mapping.fidelity} reason={row.mapping.reason} />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : null}
    </details>
  );
}
