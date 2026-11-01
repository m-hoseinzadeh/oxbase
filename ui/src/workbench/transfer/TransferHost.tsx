/** Shows the import wizard or the export dialog opened through `./store`. */
import { ExportDialog } from "./ExportDialog";
import { ImportWizard } from "./ImportWizard";
import { closeTransferDialog, useTransferDialog } from "./store";

export function TransferHost() {
  const dialog = useTransferDialog();
  if (!dialog) return null;
  if (dialog.kind === "import") {
    return <ImportWizard key={dialog.key} init={dialog.init} onClose={closeTransferDialog} />;
  }
  return <ExportDialog key={dialog.key} init={dialog.init} onClose={closeTransferDialog} />;
}
