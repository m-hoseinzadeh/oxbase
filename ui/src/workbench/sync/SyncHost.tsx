/** Shows the data transfer wizard or the backup / restore dialog opened through `./store`. */
import { BackupDialog } from "./BackupDialog";
import { RestoreDialog } from "./RestoreDialog";
import { closeSyncDialog, useSyncDialog } from "./store";
import { TransferWizard } from "./TransferWizard";

export function SyncHost() {
  const dialog = useSyncDialog();
  if (!dialog) return null;
  switch (dialog.kind) {
    case "transfer":
      return <TransferWizard key={dialog.key} init={dialog.init} onClose={closeSyncDialog} />;
    case "backup":
      return <BackupDialog key={dialog.key} init={dialog.init} onClose={closeSyncDialog} />;
    case "restore":
      return <RestoreDialog key={dialog.key} init={dialog.init} onClose={closeSyncDialog} />;
  }
}
