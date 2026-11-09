/**
 * Requests to the Connections panel from elsewhere in the shell (app bar, welcome screen, and
 * 1.3 query tabs and files opened from the OS).
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

type ConnectListener = (profileId: string) => void;

const connectListeners = new Set<ConnectListener>();

/**
 * Asks the Connections panel to connect saved profile `profileId` (with its password prompt,
 * if any): a query tab restored or opened on a connection that is not open. Returns false if
 * no panel is mounted.
 */
export function requestConnect(profileId: string): boolean {
  for (const listener of connectListeners) listener(profileId);
  return connectListeners.size > 0;
}

export function onConnectRequest(listener: ConnectListener): () => void {
  connectListeners.add(listener);
  return () => connectListeners.delete(listener);
}
