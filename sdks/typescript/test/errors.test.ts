import { describe, expect, test } from "bun:test";

import { fieldPath, retryAfterSeconds } from "../src/errors";
import {
  APIConnectionError,
  APIError,
  Norbelys,
  NorbelysError,
  PollTimeoutError,
  WebhookVerificationError,
} from "../src/index";
import { fakeFetch, respond } from "./support";

const failure = async (answer: Response): Promise<APIError> => {
  const { fetch } = fakeFetch(answer);
  const error = await new Norbelys({
    apiKey: "ak_test",
    fetch,
    maxRetries: 0,
  }).groups
    .create({ name: "VIP" })
    .catch((error_: unknown) => error_);
  if (!(error instanceof APIError)) {
    throw new Error("expected an APIError");
  }
  return error;
};

describe("APIError", () => {
  test("exposes an RFC 9457 problem with a stable code and form-ready fields", async () => {
    const error = await failure(
      respond(422, {
        code: "validation_failed",
        detail: "The request body is invalid.",
        errors: [
          {
            code: "length",
            detail: "length is lower than 1",
            pointer: "#/sequences/0/name",
          },
          {
            code: "format",
            detail: "must be an email",
            pointer: "#/recipient/email",
          },
          {
            code: "invalid",
            detail: "applies to the whole body",
            pointer: "#",
          },
        ],
        request_id: "req_1",
        status: 422,
        title: "Invalid request",
        type: "https://docs.norbelys.com/errors/validation_failed",
      })
    );

    expect(error.status).toBe(422);
    expect(error.code).toBe("validation_failed");
    expect(error.type).toBe(
      "https://docs.norbelys.com/errors/validation_failed"
    );
    expect(error.title).toBe("Invalid request");
    expect(error.detail).toBe("The request body is invalid.");
    expect(error.requestId).toBe("req_1");
    expect(error.errors).toHaveLength(3);
    expect(error.fields).toEqual({
      "recipient.email": "must be an email",
      "sequences[0].name": "length is lower than 1",
    });
    expect(error.message).toBe(
      "validation_failed: The request body is invalid. (request req_1)"
    );
  });

  test("takes the request id from X-Request-Id when the body has none", async () => {
    const error = await failure(
      respond(409, { code: "conflict" }, { "x-request-id": "req_header" })
    );

    expect(error.requestId).toBe("req_header");
    expect(error.headers.get("x-request-id")).toBe("req_header");
  });

  test("falls back to the title, then to the status, for its message", async () => {
    const titled = await failure(
      respond(409, { code: "conflict", title: "Conflict" })
    );
    const bare = await failure(respond(409, {}));

    expect(titled.message).toBe("conflict: Conflict");
    expect(bare.message).toBe("409: The API returned HTTP 409.");
    expect(bare.code).toBeUndefined();
  });

  test("keeps a non-JSON answer, such as a proxy's error page", async () => {
    const error = await failure(
      new Response("<h1>Bad gateway</h1>", {
        headers: { "content-type": "text/html" },
        status: 502,
      })
    );

    expect(error.status).toBe(502);
    expect(error.code).toBeUndefined();
    expect(error.body).toBe("<h1>Bad gateway</h1>");
    expect(error.fields).toEqual({});
  });

  test("reads Retry-After in seconds or as an HTTP date", async () => {
    const seconds = await failure(
      respond(429, { code: "rate_limited" }, { "retry-after": "30" })
    );
    const date = await failure(
      respond(
        503,
        { code: "service_unavailable" },
        { "retry-after": new Date(Date.now() + 90_000).toUTCString() }
      )
    );

    expect(seconds.retryAfter).toBe(30);
    expect(date.retryAfter).toBeGreaterThan(85);
    expect(date.retryAfter).toBeLessThanOrEqual(90);
  });

  test("ignores malformed error members", async () => {
    const error = await failure(
      respond(422, { code: 42, errors: "none", request_id: ["x"] })
    );

    expect(error.code).toBeUndefined();
    expect(error.errors).toEqual([]);
    expect(error.requestId).toBeUndefined();
  });

  test("a malformed field problem never hides the HTTP error", () => {
    const error = new APIError(
      422,
      {
        errors: [
          null,
          "bad",
          {},
          { pointer: 2, detail: "bad" },
          { pointer: "#/email", detail: 3 },
          { pointer: "#/email", detail: "bad", code: 4 },
          { pointer: "#/email", detail: "invalid", code: "invalid" },
        ],
      },
      new Headers()
    );
    expect(error.fields).toEqual({ email: "invalid" });
    expect(error.errors).toHaveLength(1);
    for (const body of [null, []]) {
      expect(new APIError(502, body, new Headers()).body).toBe(body);
    }
  });
});

describe("error classes", () => {
  test("every error the SDK throws is a NorbelysError with its own name", () => {
    const errors = [
      new APIError(500, {}, new Headers()),
      new APIConnectionError("offline", { cause: undefined, timeout: false }),
      new PollTimeoutError({ status: "Processing" }),
      new WebhookVerificationError("unsigned"),
    ];

    for (const error of errors) {
      expect(error).toBeInstanceOf(NorbelysError);
      expect(error).toBeInstanceOf(Error);
    }
    expect(errors.map((error) => error.name)).toEqual([
      "APIError",
      "APIConnectionError",
      "PollTimeoutError",
      "WebhookVerificationError",
    ]);
  });

  test("APIConnectionError keeps its cause", () => {
    const cause = new TypeError("fetch failed");

    const error = new APIConnectionError("offline", { cause, timeout: true });

    expect(error.cause).toBe(cause);
    expect(error.timeout).toBe(true);
  });
});

describe("fieldPath", () => {
  test.each([
    ["#/name", "name"],
    ["#/sequences/0/name", "sequences[0].name"],
    ["#/sequences/0/variants/12/subject", "sequences[0].variants[12].subject"],
    ["/recipient/email", "recipient.email"],
    ["#/source/data/3/email", "source.data[3].email"],
    ["#/custom_fields/a~1b", "custom_fields.a/b"],
    ["#/custom_fields/a~0b", "custom_fields.a~b"],
    ["#/0/email", "[0].email"],
  ])("maps %p to %p", (pointer, path) => {
    expect(fieldPath(pointer)).toBe(path);
  });

  test.each(["#", "", "/"])(
    "has no field for the whole body (%p)",
    (pointer) => {
      expect(fieldPath(pointer)).toBeUndefined();
    }
  );
});

describe("retryAfterSeconds", () => {
  test.each([
    ["0", 0],
    ["30", 30],
    ["1.5", 1.5],
    ["-4", 0],
  ])("reads %p as %p seconds", (value, seconds) => {
    expect(retryAfterSeconds(value)).toBe(seconds);
  });

  test("reads an HTTP date relative to now, never negative", () => {
    const past = new Date(Date.now() - 60_000).toUTCString();

    expect(retryAfterSeconds(past)).toBe(0);
  });

  test.each([null, "", "soon"])("has no delay for %p", (value) => {
    expect(retryAfterSeconds(value)).toBeUndefined();
  });
});
