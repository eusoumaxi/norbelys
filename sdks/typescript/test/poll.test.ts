import { describe, expect, spyOn, test } from "bun:test";

import { poll, PollTimeoutError } from "../src/index";

const job = (
  statuses: string[]
): { read: () => Promise<{ status: string }>; reads: () => number } => {
  let reads = 0;
  return {
    read: () => {
      const status = statuses[Math.min(reads, statuses.length - 1)] ?? "";
      reads += 1;
      return Promise.resolve({ status });
    },
    reads: () => reads,
  };
};

describe("poll", () => {
  test("returns the resource once it is done", async () => {
    const { read, reads } = job(["Queued", "Processing", "Completed"]);

    const done = await poll(read, (value) => value.status === "Completed", {
      intervalMs: 1,
    });

    expect(done.status).toBe("Completed");
    expect(reads()).toBe(3);
  });

  test("does not wait when the first read is already done", async () => {
    const { read, reads } = job(["Completed"]);

    await poll(read, (value) => value.status === "Completed");

    expect(reads()).toBe(1);
  });

  test("gives the last value on timeout, so the caller can check later", async () => {
    const { read } = job(["Processing"]);

    const error = await poll(read, () => false, {
      intervalMs: 1,
      timeoutMs: 5,
    }).catch((error_: unknown) => error_);

    expect(error).toBeInstanceOf(PollTimeoutError);
    expect((error as PollTimeoutError<{ status: string }>).last).toEqual({
      status: "Processing",
    });
  });

  test("waits longer between reads, up to maxIntervalMs", async () => {
    const waits: number[] = [];
    const timer = spyOn(globalThis, "setTimeout").mockImplementation(((
      handler: () => void,
      ms?: number
    ) => {
      waits.push(ms ?? 0);
      queueMicrotask(handler);
      return 0 as unknown as ReturnType<typeof setTimeout>;
    }) as typeof setTimeout);
    try {
      const { read } = job(["a", "b", "c", "d", "Completed"]);

      await poll(read, (value) => value.status === "Completed", {
        intervalMs: 20,
        maxIntervalMs: 40,
      });
    } finally {
      timer.mockRestore();
    }

    expect(waits).toEqual([20, 30, 40, 40]);
  });

  test("stops when its signal aborts", async () => {
    const controller = new AbortController();
    const { read } = job(["Processing"]);

    const pending = poll(read, () => false, {
      intervalMs: 50,
      signal: controller.signal,
    });
    setTimeout(() => controller.abort(new Error("user left")), 5);

    await expect(pending).rejects.toThrow("user left");
  });

  test("propagates a failing read at once", async () => {
    let reads = 0;
    const read = (): Promise<never> => {
      reads += 1;
      return Promise.reject(new Error("not found"));
    };

    await expect(poll(read, () => true)).rejects.toThrow("not found");
    expect(reads).toBe(1);
  });
});
