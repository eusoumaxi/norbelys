import {
  APIConnectionError,
  APIError,
  NorbelysError,
  retryAfterSeconds,
} from "./errors";

type MaybePromise<T> = T | Promise<T>;

/** The subset of `fetch` the SDK needs; the platform `fetch` fits it. */
export type Fetch = (input: string, init: RequestInit) => Promise<Response>;

export interface ClientOptions {
  /**
   * Workspace API key (`nb_live_…` or `nb_test_…`) for servers, scripts and CI. Defaults to the `NORBELYS_API_KEY`
   * environment variable. API keys are secrets: never ship one to a browser.
   */
  apiKey?: string;
  /**
   * Returns a bearer token for each request, the current `nbs_` workspace token. Use this in browsers:
   * it is called again on every attempt, so short-lived session tokens stay fresh.
   */
  token?: () => MaybePromise<string | null | undefined>;
  /** Defaults to `https://api.norbelys.com`. Use it for a self-hosted Norbelys or a proxy such as `/api`. */
  baseUrl?: string;
  /** Milliseconds before an attempt, including reading its response, is abandoned. Default 60 000. */
  timeoutMs?: number;
  /** Retries of requests that are safe to repeat. Default 2. */
  maxRetries?: number;
  /** Headers sent with every request. */
  headers?: Record<string, string>;
  /** Custom `fetch`, for tests or instrumented runtimes. */
  fetch?: Fetch;
  /** Allow `apiKey` in a browser, for trusted local tools only. */
  dangerouslyAllowBrowser?: boolean;
}

export interface RequestOptions {
  /**
   * Operations that accept `Idempotency-Key` get a generated key by default. Pass your own to make
   * a logical action safe to repeat later, for example after a crash. Reuse it with the same body.
   */
  idempotencyKey?: string;
  /** Cancels the request, its retries and, for lists, the following pages. */
  signal?: AbortSignal;
  /** Milliseconds before an attempt is abandoned; overrides the client's `timeoutMs`. */
  timeoutMs?: number;
  /** Retries of this request when it is safe to repeat; overrides the client's `maxRetries`. */
  maxRetries?: number;
  /** Headers for this request, over the client's own. */
  headers?: Record<string, string>;
}

/** The options of an update (`PATCH`): those of every request, and its precondition. */
export interface UpdateOptions extends RequestOptions {
  /**
   * The `version` of the resource this update was prepared from, sent as `If-Match`. When the
   * resource changed since (another edit, or background work such as a health check), the API
   * applies nothing and answers `412 precondition_failed`: read it again and reapply the change.
   * A number or a string of digits is sent as the strong entity tag `"<version>"`; any other
   * string (`*`, an `ETag` as received) is sent as it is.
   *
   * ```ts
   * const group = await norbelys.groups.retrieve(id);
   * await norbelys.groups.update(id, { name: "Customers" }, { ifMatch: group.version });
   * ```
   */
  ifMatch?: number | string;
}

/** The `If-Match` value for `version`: a version is quoted, anything else is sent as it is. */
export const ifMatchHeader = (version: number | string): string => {
  const text = String(version).trim();
  return /^\d+$/u.test(text) ? `"${text}"` : text;
};

type KeysOf<T> = T extends unknown ? keyof T : never;

/**
 * Exactly one form of a request body union: the members of the other forms become `never`.
 * The API rejects mixed forms, and a plain TypeScript union would accept them.
 */
export type OneOf<All, Form extends All = All> = Form extends unknown
  ? Form & Partial<Record<Exclude<KeysOf<All>, keyof Form>, never>>
  : never;

/** One HTTP operation of the contract. Generated code passes it; you do not need it. */
export interface Route {
  method: "GET" | "POST" | "PUT" | "PATCH" | "DELETE";
  path: string;
  /** The operation accepts `Idempotency-Key`, so repeating it with the same key is safe. */
  idempotent: boolean;
}

/** A finished attempt: the response and its fully read, parsed body. */
interface Answer {
  response: Response;
  data: unknown;
}

const DEFAULT_BASE_URL = "https://api.norbelys.com";
const RETRY_STATUSES = new Set([408, 429, 500, 502, 503, 504]);
const MAX_RETRY_DELAY_MS = 60_000;

const readEnv = (name: string): string | undefined => {
  const env = (
    globalThis as { process?: { env?: Record<string, string | undefined> } }
  ).process?.env;
  return env?.[name] || undefined;
};

const isBrowser = (): boolean =>
  (globalThis as { document?: unknown }).document !== undefined;

const resolveBaseUrl = (baseUrl: string): string => {
  const trimmed = baseUrl.replace(/\/+$/u, "");
  if (!trimmed.startsWith("/")) {
    return trimmed;
  }
  const origin = (globalThis as { location?: { origin?: string } }).location
    ?.origin;
  if (!origin) {
    throw new NorbelysError(
      `The relative baseUrl "${baseUrl}" needs a browser location.`
    );
  }
  return `${origin}${trimmed}`;
};

/** JSON when the body is JSON; the raw text otherwise (a proxy's HTML error page, for example). */
const parseBody = (text: string): unknown => {
  if (!text) {
    return undefined;
  }
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
};

/** Browser abort payloads have no prescribed shape; retain them as unknown. */
export const abortReason = (signal?: { readonly reason: unknown }): unknown =>
  signal?.reason;
const isQueryArray = (value: unknown): value is readonly unknown[] =>
  Array.isArray(value);

/** Resolves after `ms`, or rejects with the signal's reason when it aborts first. Leaves nothing behind. */
export const sleep = async (
  ms: number,
  signal?: AbortSignal
): Promise<void> => {
  if (signal?.aborted) {
    throw abortReason(signal);
  }
  let onAbort: (() => void) | undefined;
  try {
    // oxlint-disable-next-line promise/avoid-new -- a timer needs the Promise constructor
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(resolve, ms);
      onAbort = () => {
        clearTimeout(timer);
        reject(abortReason(signal));
      };
      signal?.addEventListener("abort", onAbort, { once: true });
    });
  } finally {
    if (onAbort) {
      signal?.removeEventListener("abort", onAbort);
    }
  }
};

const backoff = (attempt: number): number =>
  Math.min(8000, 500 * 2 ** attempt) * (0.75 + Math.random() * 0.25);

/**
 * Whether the same request may succeed when sent again: a retryable status, or the first request
 * with this `Idempotency-Key` still running (`409 idempotency_in_progress`), whose answer the
 * same key replays once it ends.
 */
const retryable = ({ response, data }: Answer): boolean =>
  RETRY_STATUSES.has(response.status) ||
  (response.status === 409 &&
    typeof data === "object" &&
    data !== null &&
    (data as { code?: unknown }).code === "idempotency_in_progress");

/** Delay before the next attempt, or `undefined` when this outcome must not be retried. */
const retryDelay = (
  outcome: Answer | Error,
  attempt: number
): number | undefined => {
  if (outcome instanceof Error) {
    return backoff(attempt);
  }
  if (!retryable(outcome)) {
    return undefined;
  }
  const seconds = retryAfterSeconds(
    outcome.response.headers.get("retry-after")
  );
  if (seconds === undefined) {
    return backoff(attempt);
  }
  // A longer wait than a request should block for is left to the caller (`APIError.retryAfter`).
  return seconds * 1000 > MAX_RETRY_DELAY_MS ? undefined : seconds * 1000;
};

const failure = (outcome: Answer | Error): NorbelysError => {
  if (!(outcome instanceof Error)) {
    return new APIError(
      outcome.response.status,
      outcome.data,
      outcome.response.headers
    );
  }
  const timeout = outcome.name === "TimeoutError";
  return new APIConnectionError(
    timeout
      ? "The request timed out."
      : "The request could not reach the Norbelys API.",
    { cause: outcome, timeout }
  );
};

/** Sends requests for the generated resources. Safe to share across the whole application. */
export class Core {
  readonly #apiKey: string | undefined;
  readonly #token: ClientOptions["token"];
  readonly #baseUrl: string;
  readonly #timeoutMs: number;
  readonly #maxRetries: number;
  readonly #headers: Readonly<Record<string, string>>;
  readonly #fetch: Fetch;

  constructor(options: ClientOptions) {
    const apiKey =
      options.apiKey ??
      (options.token ? undefined : readEnv("NORBELYS_API_KEY"));
    if (apiKey && isBrowser() && !options.dangerouslyAllowBrowser) {
      throw new NorbelysError(
        "API keys are secrets and must not run in a browser. Pass `token` (a workspace-token callback) instead."
      );
    }
    if (!apiKey && !options.token) {
      throw new NorbelysError(
        "Missing credentials: pass `apiKey` on servers, `token` in browsers, or set NORBELYS_API_KEY."
      );
    }
    this.#apiKey = apiKey;
    this.#token = options.token;
    this.#baseUrl = resolveBaseUrl(options.baseUrl ?? DEFAULT_BASE_URL);
    this.#timeoutMs = options.timeoutMs ?? 60_000;
    this.#maxRetries = options.maxRetries ?? 2;
    this.#headers = { ...options.headers };
    this.#fetch = options.fetch ?? ((input, init) => fetch(input, init));
  }

  async request<T>(
    route: Route,
    pathArgs: readonly string[],
    input: {
      query?: object | undefined;
      body?: unknown;
      rawBody?: string | Blob;
      contentType?: string;
    },
    options: UpdateOptions = {}
  ): Promise<T> {
    const url = this.#url(route.path, pathArgs, input.query);
    const body =
      input.rawBody ??
      (input.body === undefined ? undefined : JSON.stringify(input.body));
    const idempotencyKey = route.idempotent
      ? (options.idempotencyKey ?? crypto.randomUUID())
      : options.idempotencyKey;
    // Writes are repeated only when the contract makes the repetition safe.
    const repeatable =
      route.method === "GET" || route.method === "DELETE" || route.idempotent;
    const maxRetries = repeatable
      ? (options.maxRetries ?? this.#maxRetries)
      : 0;

    for (let attempt = 0; ; attempt += 1) {
      // Outside the attempt: a failing `token` callback is the caller's error, never retried.
      // oxlint-disable-next-line no-await-in-loop -- a fresh token for every attempt
      const headers = await this.#requestHeaders(
        body === undefined
          ? undefined
          : (input.contentType ?? "application/json"),
        idempotencyKey,
        options
      );
      // oxlint-disable-next-line no-await-in-loop -- attempts are sequential by design
      const outcome = await this.#attempt(
        route.method,
        url,
        body,
        headers,
        options
      );
      if (!(outcome instanceof Error) && outcome.response.ok) {
        return outcome.data as T;
      }
      const delay =
        attempt < maxRetries ? retryDelay(outcome, attempt) : undefined;
      if (delay === undefined) {
        throw failure(outcome);
      }
      // oxlint-disable-next-line no-await-in-loop -- backoff between sequential attempts
      await sleep(delay, options.signal);
    }
  }

  /**
   * One attempt, response body included, under one timeout. Returns the transport error instead
   * of throwing it; a caller's cancellation is rethrown. Its timer and listener never outlive it.
   */
  async #attempt(
    method: Route["method"],
    url: string,
    body: string | Blob | undefined,
    headers: Headers,
    options: RequestOptions
  ): Promise<Answer | Error> {
    const { signal } = options;
    if (signal?.aborted) {
      throw abortReason(signal);
    }
    const controller = new AbortController();
    const onAbort = () => controller.abort(abortReason(signal));
    const timer = setTimeout(
      () =>
        controller.abort(
          new DOMException("The request timed out.", "TimeoutError")
        ),
      options.timeoutMs ?? this.#timeoutMs
    );
    signal?.addEventListener("abort", onAbort, { once: true });
    try {
      const response = await this.#fetch(url, {
        method,
        headers,
        body,
        signal: controller.signal,
      });
      return { response, data: parseBody(await response.text()) };
    } catch (error) {
      if (signal?.aborted) {
        throw abortReason(signal);
      }
      return error instanceof Error ? error : new Error(String(error));
    } finally {
      clearTimeout(timer);
      signal?.removeEventListener("abort", onAbort);
    }
  }

  async #requestHeaders(
    contentType: string | undefined,
    idempotencyKey: string | undefined,
    options: UpdateOptions
  ): Promise<Headers> {
    const headers = new Headers({ ...this.#headers, ...options.headers });
    headers.set("accept", "application/json");
    if (options.ifMatch !== undefined) {
      headers.set("if-match", ifMatchHeader(options.ifMatch));
    }
    const token = this.#apiKey ?? (await this.#token?.());
    if (token) {
      headers.set("authorization", `Bearer ${token}`);
    }
    if (contentType) {
      headers.set("content-type", contentType);
    }
    if (idempotencyKey) {
      headers.set("idempotency-key", idempotencyKey);
    }
    return headers;
  }

  #url(
    path: string,
    pathArgs: readonly string[],
    query: object | undefined
  ): string {
    let index = 0;
    const filled = path.replaceAll(/\{[^}]+\}/gu, (placeholder) => {
      const value = pathArgs[index];
      index += 1;
      if (value === undefined || value === "") {
        throw new NorbelysError(
          `Missing path parameter ${placeholder} for ${path}.`
        );
      }
      return encodeURIComponent(value);
    });
    const search = new URLSearchParams();
    for (const [key, value] of Object.entries(
      (query ?? {}) as Record<string, unknown>
    )) {
      for (const item of isQueryArray(value) ? value : [value]) {
        if (item !== undefined && item !== null) {
          search.append(
            key,
            typeof item === "object" ? JSON.stringify(item) : String(item)
          );
        }
      }
    }
    const qs = search.toString();
    return `${this.#baseUrl}${filled}${qs ? `?${qs}` : ""}`;
  }
}
