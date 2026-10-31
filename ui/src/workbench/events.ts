/** Workbench-wide notifications between panels that do not share React state. */

type Listener = () => void;

const historyListeners = new Set<Listener>();

/** Called when a run ended, so the History panel can reload. */
export function notifyHistoryChanged(): void {
  for (const listener of historyListeners) listener();
}

export function onHistoryChanged(listener: Listener): () => void {
  historyListeners.add(listener);
  return () => {
    historyListeners.delete(listener);
  };
}
