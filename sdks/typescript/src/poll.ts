import { sleep } from "./core";
import { PollTimeoutError } from "./errors";

export interface PollOptions {
  /** First wait between reads. Default 1 000 ms; it grows by 1.5x up to `maxIntervalMs`. */
  intervalMs?: number;
  /** Default 10 000 ms. */
  maxIntervalMs?: number;
  /** Default 120 000 ms. On timeout, `PollTimeoutError.last` holds the latest value. */
  timeoutMs?: number;
  signal?: AbortSignal;
}

/**
 * Reads a resource until `isDone` accepts it. Terminal failures count as done: check the status.
 *
 * ```ts
 * const done = await poll(
 *   () => norbelys.imports.retrieve(id),
 *   (i) => i.status === "completed" || i.status === "failed",
 * );
 * ```
 */
export const poll = async <T>(
  read: () => Promise<T>,
  isDone: (value: T) => boolean,
  options: PollOptions = {}
): Promise<T> => {
  const deadline = Date.now() + (options.timeoutMs ?? 120_000);
  let interval = options.intervalMs ?? 1000;
  for (;;) {
    // oxlint-disable-next-line no-await-in-loop -- each read depends on the previous one
    const value = await read();
    if (isDone(value)) {
      return value;
    }
    if (Date.now() + interval > deadline) {
      throw new PollTimeoutError(value);
    }
    // oxlint-disable-next-line no-await-in-loop -- wait between sequential reads
    await sleep(interval, options.signal);
    interval = Math.min(interval * 1.5, options.maxIntervalMs ?? 10_000);
  }
};
