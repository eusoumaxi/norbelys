import { abortReason } from "../src/core";
import type { Fetch } from "../src/core";

/** One request the fake `fetch` received. */
export interface Call {
  url: string;
  init: RequestInit;
}

/** A JSON response. */
export const respond = (
  status: number,
  body?: unknown,
  headers: Record<string, string> = {}
): Response =>
  new Response(body === undefined ? null : JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", ...headers },
  });

/** A retryable failure that asks for no wait. */
export const unavailable = (): Response =>
  respond(503, { code: "service_unavailable" }, { "retry-after": "0" });

/** The last page of any list. */
export const lastPage = (data: unknown[] = []): Response =>
  respond(200, { data, meta: { has_more: false, next_cursor: null } });

/**
 * A `fetch` that answers from a queue and records every call. An `Error` in the queue fails the
 * request like a network error; an empty queue fails any unexpected request.
 */
export const fakeFetch = (
  ...answers: (Response | Error)[]
): { fetch: Fetch; calls: Call[] } => {
  const calls: Call[] = [];
  const fetch: Fetch = (url, init) => {
    calls.push({ url, init });
    const answer = answers.shift();
    if (answer instanceof Error) {
      return Promise.reject(answer);
    }
    return answer
      ? Promise.resolve(answer)
      : Promise.reject(new Error(`Unexpected request to ${url}`));
  };
  return { fetch, calls };
};

/** A header of a recorded call. */
export const header = (call: Call | undefined, name: string): string | null =>
  new Headers(call?.init.headers).get(name);

/** The parsed JSON body of a recorded call. */
export const jsonBody = (call: Call | undefined): unknown =>
  typeof call?.init.body === "string" ? JSON.parse(call.init.body) : undefined;

/** A request that only ends when it is aborted. */
export const hang: Fetch = (_url, init) =>
  // oxlint-disable-next-line promise/avoid-new -- the request settles only through its signal
  new Promise((_resolve, reject) => {
    init.signal?.addEventListener("abort", () =>
      reject(abortReason(init.signal ?? undefined))
    );
  });

const setEnv = (name: string, value: string | undefined): void => {
  if (value === undefined) {
    Reflect.deleteProperty(process.env, name);
  } else {
    process.env[name] = value;
  }
};

/** Runs `run` with an environment variable set, or removed with `undefined`, then restores it. */
export const withEnv = async <T>(
  name: string,
  value: string | undefined,
  run: () => T | Promise<T>
): Promise<T> => {
  const saved = process.env[name];
  setEnv(name, value);
  try {
    return await run();
  } finally {
    setEnv(name, saved);
  }
};

/** Runs `run` as if on a browser page of `origin`, then restores the globals. */
export const inBrowser = <T>(
  run: () => T,
  origin = "https://app.example.com"
): T => {
  const scope = globalThis as { document?: unknown; location?: unknown };
  const saved = { document: scope.document, location: scope.location };
  scope.document = {};
  scope.location = { origin };
  try {
    return run();
  } finally {
    for (const [key, value] of Object.entries(saved)) {
      if (value === undefined) {
        Reflect.deleteProperty(scope, key);
      } else {
        scope[key as keyof typeof saved] = value;
      }
    }
  }
};
