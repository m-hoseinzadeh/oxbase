/**
 * One run at a time on an editor session. The core refuses a second run on an editor that is
 * still running one (`invalid_input`), and it frees the editor before it sends the run's
 * terminal event. So a panel that starts a new run (a new scan, a value read again) while the
 * last one may still stream takes the slot: that stops the previous run (`query_cancel`) and
 * waits for its terminal event before the new run opens.
 */
import { isTerminalEvent, queryCancel, type RunEvent } from "../../api";

/** Longest wait for a stopped run's end before the next run tries anyway. */
export const STOP_WAIT_MS = 10_000;

interface Holder {
  done: Promise<void>;
  release: () => void;
  runId: string | null;
  stopped: boolean;
}

/** What the run that took the slot reports back. */
export interface RunTicket {
  /** Every event of the run (the terminal one frees the slot). */
  event: (event: RunEvent) => void;
  /** The run's id, once its open returned (a stop asked before then cancels it now). */
  started: (runId: string) => void;
  /** No run holds the editor (the open failed, or the caller gave up before opening). */
  release: () => void;
}

function stop(holder: Holder): void {
  holder.stopped = true;
  if (holder.runId) void queryCancel(holder.runId).catch(() => {});
}

export class EditorRunSlot {
  private holder: Holder | null = null;

  /** Stops the run that holds the slot, waits for its end, and hands the slot over. */
  async take(): Promise<RunTicket> {
    const previous = this.holder;
    let release = () => {};
    const done = new Promise<void>((resolve) => {
      release = resolve;
    });
    const holder: Holder = { done, release, runId: null, stopped: false };
    this.holder = holder;
    if (previous) {
      stop(previous);
      let timer: ReturnType<typeof setTimeout> | undefined;
      await Promise.race([
        previous.done,
        new Promise<void>((resolve) => {
          timer = setTimeout(resolve, STOP_WAIT_MS);
        }),
      ]);
      clearTimeout(timer);
    }
    return {
      event: (event) => {
        if (isTerminalEvent(event)) holder.release();
      },
      started: (runId) => {
        holder.runId = runId;
        if (holder.stopped) void queryCancel(runId).catch(() => {});
      },
      release: () => holder.release(),
    };
  }

  /** Stops the run that holds the slot (a newer request will take it). */
  stop(): void {
    if (this.holder) stop(this.holder);
  }
}
