import { useEffect, useState } from "react";

import { describeApiError } from "./api.js";

export type AsyncState<T> =
  | { status: "loading" }
  | { status: "ready"; value: T }
  | { status: "error"; message: string };

/**
 * Loads once per dependency change and drops results from superseded loads, so a
 * slow response for an old filter cannot overwrite the current one.
 */
export function useAsync<T>(load: () => Promise<T>, deps: unknown[]): AsyncState<T> {
  const [state, setState] = useState<AsyncState<T>>({ status: "loading" });

  useEffect(() => {
    let current = true;
    setState({ status: "loading" });
    load()
      .then((value) => {
        if (current) setState({ status: "ready", value });
      })
      .catch((thrown: unknown) => {
        if (!current) return;
        setState({
          status: "error",
          message: describeApiError(thrown, "加载失败，请重试。"),
        });
      });
    return () => {
      current = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);

  return state;
}
