/**
 * Requests to the Connections panel from elsewhere in the shell (app bar, welcome screen).
 * The panel owns its dialogs; this only asks it to open one.
 */
type Listener = () => void;

const newConnectionListeners = new Set<Listener>();

/** Asks the Connections panel to open the "new connection" dialog. Returns false if no panel is mounted. */
export function requestNewConnection(): boolean {
  for (const listener of newConnectionListeners) listener();
  return newConnectionListeners.size > 0;
}

export function onNewConnectionRequest(listener: Listener): () => void {
  newConnectionListeners.add(listener);
  return () => newConnectionListeners.delete(listener);
}
