import { describe, expect, spyOn, test } from "bun:test";

import type { Fetch } from "../src/core";
import { APIConnectionError, APIError, Norbelys } from "../src/index";
import { fakeFetch, hang, header, respond, unavailable } from "./support";

const client = (fetch: Fetch, maxRetries?: number): Norbelys =>
  new Norbelys({ apiKey: "ak_test", fetch, maxRetries });

describe("retries", () => {
  test("repeats a write that accepts Idempotency-Key with the same key", async () => {
    const { fetch, calls } = fakeFetch(
      unavailable(),
      respond(202, { id: "msg_1" })
    );

    await client(fetch).messages.create({
      person_id: "per_1",
      step_id: "stp_1",
      to: "ada@example.com",
    });

    expect(calls).toHaveLength(2);
    expect(header(calls[0], "idempotency-key")).toBeString();
    expect(header(calls[1], "idempotency-key")).toBe(
      header(calls[0], "idempotency-key")
    );
  });

  test("gives every logical write its own generated key", async () => {
    const { fetch, calls } = fakeFetch(
      respond(202, { id: "msg_1" }),
      respond(202, { id: "msg_2" })
    );
    const norbelys = client(fetch);

    await norbelys.messages.create({ person_id: "per_1", step_id: "stp_1" });
    await norbelys.messages.create({ person_id: "per_2", step_id: "stp_1" });

    expect(header(calls[0], "idempotency-key")).not.toBe(
      header(calls[1], "idempotency-key")
    );
  });

  test("sends a caller's idempotency key verbatim", async () => {
    const { fetch, calls } = fakeFetch(respond(202, { id: "msg_1" }));

    await client(fetch).messages.create(
      { person_id: "per_1", step_id: "stp_1" },
      { idempotencyKey: "welcome-per_1" }
    );

    expect(header(calls[0], "idempotency-key")).toBe("welcome-per_1");
  });

  test("waits out a keyed write still running, then gets its answer with the same key", async () => {
    const { fetch, calls } = fakeFetch(
      respond(409, { code: "idempotency_in_progress" }, { "retry-after": "0" }),
      respond(201, { id: "grp_1" })
    );

    const group = await client(fetch).groups.create({ name: "VIP" });

    expect(group.id).toBe("grp_1");
    expect(calls).toHaveLength(2);
    expect(header(calls[1], "idempotency-key")).toBe(
      header(calls[0], "idempotency-key")
    );
  });

  test("fails at once on any other conflict", async () => {
    const { fetch, calls } = fakeFetch(
      respond(409, { code: "invalid_state" }, { "retry-after": "0" })
    );

    await expect(client(fetch).campaigns.start("cmp_1")).rejects.toMatchObject({
      code: "invalid_state",
      status: 409,
    });
    expect(calls).toHaveLength(1);
  });

  test("never repeats a POST without an idempotency guarantee", async () => {
    const { fetch, calls } = fakeFetch(unavailable());

    await expect(
      client(fetch).preflight.create({ emails: ["ada@example.com"] })
    ).rejects.toBeInstanceOf(APIError);
    expect(calls).toHaveLength(1);
    expect(header(calls[0], "idempotency-key")).toBeNull();
  });

  test("retries reads until they succeed", async () => {
    const { fetch, calls } = fakeFetch(
      unavailable(),
      unavailable(),
      respond(200, { id: "grp_1" })
    );

    const group = await client(fetch).groups.retrieve("grp_1");

    expect(group.id).toBe("grp_1");
    expect(calls).toHaveLength(3);
  });

  test("retries deletes, which are idempotent by definition", async () => {
    const { fetch, calls } = fakeFetch(unavailable(), respond(204));

    await client(fetch).groups.delete("grp_1");

    expect(calls.map((call) => call.init.method)).toEqual(["DELETE", "DELETE"]);
  });

  test.each([408, 429, 500, 502, 503, 504])(
    "retries a %i answer",
    async (status) => {
      const { fetch, calls } = fakeFetch(
        respond(status, {}, { "retry-after": "0" }),
        respond(200, { id: "grp_1" })
      );

      await client(fetch).groups.retrieve("grp_1");

      expect(calls).toHaveLength(2);
    }
  );

  test("gives up after two retries by default and reports the last answer", async () => {
    const { fetch, calls } = fakeFetch(
      unavailable(),
      unavailable(),
      respond(504, { code: "Timeout" }, { "retry-after": "0" })
    );

    const error = await client(fetch)
      .groups.retrieve("grp_1")
      .catch((error_: unknown) => error_);

    expect(calls).toHaveLength(3);
    expect(error).toBeInstanceOf(APIError);
    expect((error as APIError).status).toBe(504);
  });

  test("honours maxRetries on the client and on each request", async () => {
    const none = fakeFetch(unavailable());
    await expect(
      client(none.fetch, 0).groups.retrieve("grp_1")
    ).rejects.toThrow();
    expect(none.calls).toHaveLength(1);

    const one = fakeFetch(unavailable(), unavailable());
    await expect(
      client(one.fetch, 0).groups.retrieve("grp_1", { maxRetries: 1 })
    ).rejects.toThrow();
    expect(one.calls).toHaveLength(2);
  });

  test.each([400, 401, 403, 404, 409, 413, 422])(
    "fails at once on a %i answer, which a retry cannot fix",
    async (status) => {
      const { fetch, calls } = fakeFetch(respond(status, { code: "Nope" }));

      await expect(
        client(fetch).groups.retrieve("grp_1")
      ).rejects.toMatchObject({
        status,
      });
      expect(calls).toHaveLength(1);
    }
  );

  test("does not block for a Retry-After longer than a minute", async () => {
    const { fetch, calls } = fakeFetch(
      respond(429, { code: "RateLimited" }, { "retry-after": "120" })
    );

    const error = await client(fetch)
      .groups.retrieve("grp_1")
      .catch((error_: unknown) => error_);

    expect(calls).toHaveLength(1);
    expect((error as APIError).retryAfter).toBe(120);
  });

  test("retries a network failure of a read, then reports APIConnectionError", async () => {
    const cause = new TypeError("fetch failed");
    const { fetch, calls } = fakeFetch(cause, cause);

    const error = await client(fetch, 1)
      .groups.retrieve("grp_1")
      .catch((error_: unknown) => error_);

    expect(calls).toHaveLength(2);
    expect(error).toBeInstanceOf(APIConnectionError);
    expect((error as APIConnectionError).timeout).toBe(false);
    expect((error as APIConnectionError).cause).toBe(cause);
  });

  test("reports a network failure of an unsafe write at once", async () => {
    const { fetch, calls } = fakeFetch(new TypeError("fetch failed"));

    await expect(
      client(fetch).preflight.create({ emails: ["ada@example.com"] })
    ).rejects.toBeInstanceOf(APIConnectionError);
    expect(calls).toHaveLength(1);
  });
});

describe("timeouts and cancellation", () => {
  test("turns a timeout into APIConnectionError", async () => {
    const norbelys = new Norbelys({
      apiKey: "ak_test",
      fetch: hang,
      timeoutMs: 5,
    });

    const error = await norbelys.groups
      .create({ name: "VIP" })
      .catch((error_: unknown) => error_);

    expect(error).toBeInstanceOf(APIConnectionError);
    expect((error as APIConnectionError).timeout).toBe(true);
  });

  test("applies a per-request timeout", async () => {
    const norbelys = new Norbelys({ apiKey: "ak_test", fetch: hang });

    const error = await norbelys.groups
      .create({ name: "VIP" }, { timeoutMs: 5 })
      .catch((error_: unknown) => error_);

    expect((error as APIConnectionError).timeout).toBe(true);
  });

  test("retries a read that timed out", async () => {
    let attempts = 0;
    const fetch: Fetch = (url, init) => {
      attempts += 1;
      return attempts === 1
        ? hang(url, init)
        : Promise.resolve(respond(200, { id: "grp_1" }));
    };
    const norbelys = new Norbelys({
      apiKey: "ak_test",
      fetch,
      maxRetries: 1,
      timeoutMs: 5,
    });

    const group = await norbelys.groups.retrieve("grp_1");

    expect(group.id).toBe("grp_1");
    expect(attempts).toBe(2);
  });

  test("rethrows the caller's cancellation as is, without sending", async () => {
    const { fetch, calls } = fakeFetch();
    const controller = new AbortController();
    controller.abort(new Error("stopped by the caller"));

    await expect(
      client(fetch).groups.retrieve("grp_1", { signal: controller.signal })
    ).rejects.toThrow("stopped by the caller");
    expect(calls).toHaveLength(0);
  });

  test("aborts a request in flight", async () => {
    const controller = new AbortController();
    const pending = new Norbelys({
      apiKey: "ak_test",
      fetch: hang,
    }).groups.retrieve("grp_1", { signal: controller.signal });

    controller.abort(new Error("user left"));

    await expect(pending).rejects.toThrow("user left");
  });

  test("stops retrying when the caller aborts during the backoff", async () => {
    const controller = new AbortController();
    const { fetch, calls } = fakeFetch(
      respond(503, { code: "ServiceUnavailable" })
    );
    const pending = client(fetch).groups.retrieve("grp_1", {
      signal: controller.signal,
    });

    setTimeout(() => controller.abort(new Error("user left")), 20);

    await expect(pending).rejects.toThrow("user left");
    expect(calls).toHaveLength(1);
  });
});

/** Counts the listeners added to and removed from a signal. */
const tracked = (
  signal: AbortSignal
): { added: () => number; removed: () => number } => {
  const added = spyOn(signal, "addEventListener");
  const removed = spyOn(signal, "removeEventListener");
  return {
    added: () => added.mock.calls.length,
    removed: () => removed.mock.calls.length,
  };
};

describe("memory", () => {
  test("removes every listener it adds to the caller's signal, across retries", async () => {
    const controller = new AbortController();
    const listeners = tracked(controller.signal);
    const { fetch } = fakeFetch(unavailable(), respond(200, { id: "grp_1" }));

    await client(fetch).groups.retrieve("grp_1", { signal: controller.signal });

    expect(listeners.added()).toBeGreaterThan(0);
    expect(listeners.removed()).toBe(listeners.added());
  });

  test("removes its listeners after an error answer", async () => {
    const controller = new AbortController();
    const listeners = tracked(controller.signal);
    const { fetch } = fakeFetch(respond(404, { code: "NotFound" }));

    await expect(
      client(fetch).groups.retrieve("grp_1", { signal: controller.signal })
    ).rejects.toBeInstanceOf(APIError);

    expect(listeners.removed()).toBe(listeners.added());
  });

  test("removes its listeners after a timeout", async () => {
    const controller = new AbortController();
    const listeners = tracked(controller.signal);
    const norbelys = new Norbelys({
      apiKey: "ak_test",
      fetch: hang,
      maxRetries: 0,
      timeoutMs: 5,
    });

    await expect(
      norbelys.groups.retrieve("grp_1", { signal: controller.signal })
    ).rejects.toBeInstanceOf(APIConnectionError);

    expect(listeners.removed()).toBe(listeners.added());
  });

  test("clears its timeout timer once an attempt ends", async () => {
    const cleared = spyOn(globalThis, "clearTimeout");
    try {
      const { fetch } = fakeFetch(respond(200, { id: "grp_1" }));

      await client(fetch).groups.retrieve("grp_1");

      expect(cleared).toHaveBeenCalled();
    } finally {
      cleared.mockRestore();
    }
  });
});
