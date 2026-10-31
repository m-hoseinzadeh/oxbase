import { useEffect, useState } from "react";

export type AsyncState<T> =
  | { status: "loading" }
  | { status: "ok"; value: T }
  | { status: "error"; error: unknown };

/** Runs `load` once on mount and tracks its result. Ignores results after unmount. */
export function useAsync<T>(load: () => Promise<T>): AsyncState<T> {
  const [state, setState] = useState<AsyncState<T>>({ status: "loading" });
  useEffect(() => {
    let alive = true;
    load().then(
      (value) => {
        if (alive) setState({ status: "ok", value });
      },
      (error: unknown) => {
        if (alive) setState({ status: "error", error });
      },
    );
    return () => {
      alive = false;
    };
  }, [load]);
  return state;
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
