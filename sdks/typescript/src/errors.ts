/* oxlint-disable max-classes-per-file -- one small error hierarchy, imported together */
import type { FieldError, ProblemCode } from "./generated/schema";

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

/** Reject malformed proxy responses while retaining valid field problems. */
const isFieldError = (value: unknown): value is FieldError =>
  isRecord(value) &&
  typeof value.pointer === "string" &&
  typeof value.detail === "string" &&
  typeof value.code === "string";

const text = (value: unknown): string | undefined =>
  typeof value === "string" ? value : undefined;

/** `#/sequences/0/name` becomes `sequences[0].name`; the whole-body pointer `#` has no field. */
export const fieldPath = (pointer: string): string | undefined => {
  let path = "";
  for (const raw of pointer.replace(/^#/u, "").split("/").slice(1)) {
    const segment = raw.replaceAll("~1", "/").replaceAll("~0", "~");
    if (/^\d+$/u.test(segment)) {
      path += `[${segment}]`;
    } else if (path) {
      path += `.${segment}`;
    } else {
      path = segment;
    }
  }
  return path || undefined;
};

/** Seconds from a `Retry-After` value: a number of seconds or an HTTP date. */
export const retryAfterSeconds = (value: string | null): number | undefined => {
  if (!value) {
    return undefined;
  }
  const seconds = Number(value);
  if (Number.isFinite(seconds)) {
    return Math.max(0, seconds);
  }
  const date = Date.parse(value);
  return Number.isNaN(date)
    ? undefined
    : Math.max(0, (date - Date.now()) / 1000);
};

/** Base class of every error this SDK throws. */
export class NorbelysError extends Error {
  override name = "NorbelysError";
}

/**
 * The API answered with an RFC 9457 problem. Branch on `code`, never on the message: comparing
 * it narrows it to one of the registry's codes (`ProblemCode`), and the registry may grow, so
 * keep a default branch.
 *
 * ```ts
 * if (error instanceof APIError && error.code === "validation_failed") {
 *   error.fields; // { "sequences[0].name": "length is lower than 1" }
 * }
 * ```
 */
export class APIError extends NorbelysError {
  override name = "APIError";
  /** HTTP status code. */
  readonly status: number;
  /**
   * Stable machine code, such as `validation_failed` or `not_found`; `undefined` when the
   * answer is not a problem document (a proxy's error page, for example).
   */
  readonly code: ProblemCode | undefined;
  /** The page that explains the code (the problem's `type`). */
  readonly type: string | undefined;
  /** Fixed per code. */
  readonly title: string | undefined;
  /** What went wrong, for a person. */
  readonly detail: string | undefined;
  /** Same value as the `X-Request-Id` response header. Include it when reporting a problem. */
  readonly requestId: string | undefined;
  /** Invalid request members, with JSON Pointers into the request body or query. */
  readonly errors: readonly FieldError[];
  /** `errors` keyed by field path (`recipient.email`, `sequences[0].name`), ready for form libraries. */
  readonly fields: Readonly<Record<string, string>>;
  /** Seconds to wait before retrying, from `Retry-After`. */
  readonly retryAfter: number | undefined;
  readonly headers: Headers;
  /** The parsed response body. */
  readonly body: unknown;

  constructor(status: number, body: unknown, headers: Headers) {
    const problem = isRecord(body) ? body : {};
    const code = text(problem.code) as ProblemCode | undefined;
    const detail = text(problem.detail);
    const title = text(problem.title);
    const requestId =
      text(problem.request_id) ?? headers.get("x-request-id") ?? undefined;
    const reason = detail ?? title ?? `The API returned HTTP ${status}.`;
    super(
      `${code ?? status}: ${reason}${requestId ? ` (request ${requestId})` : ""}`
    );
    this.status = status;
    this.code = code;
    this.type = text(problem.type);
    this.title = title;
    this.detail = detail;
    this.requestId = requestId;
    this.errors = Array.isArray(problem.errors)
      ? problem.errors.filter(isFieldError)
      : [];
    this.fields = Object.fromEntries(
      this.errors.flatMap((problemField) => {
        const path = fieldPath(problemField.pointer);
        return path ? [[path, problemField.detail]] : [];
      })
    );
    this.retryAfter = retryAfterSeconds(headers.get("retry-after"));
    this.headers = headers;
    this.body = body;
  }
}

/** The request never produced an answer: network failure or timeout. */
export class APIConnectionError extends NorbelysError {
  override name = "APIConnectionError";
  /** Whether the request exceeded its timeout. */
  readonly timeout: boolean;

  constructor(message: string, options: { cause: unknown; timeout: boolean }) {
    super(message, { cause: options.cause });
    this.timeout = options.timeout;
  }
}

/** A webhook delivery failed verification: refuse it (answer `400`), and never act on its body. */
export class WebhookVerificationError extends NorbelysError {
  override name = "WebhookVerificationError";
}

/** `poll` ran out of time. `last` is the most recent value, so the caller can keep checking later. */
export class PollTimeoutError<T = unknown> extends NorbelysError {
  override name = "PollTimeoutError";
  readonly last: T;

  constructor(last: T) {
    super("The resource did not reach the expected state before the timeout.");
    this.last = last;
  }
}
