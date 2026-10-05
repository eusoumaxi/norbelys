import { WebhookVerificationError } from "./errors";
import type { EventType } from "./generated/schema";

/**
 * A webhook delivery's body, as an endpoint receives it. `data` carries ids and the few fields
 * needed to decide whether to fetch the object; fetch it for the rest. New event types may be
 * added, so keep a default branch when you switch on `type`.
 */
export interface WebhookEvent {
  /** What happened, such as `message.sent`. */
  type: EventType;
  /** When the event happened (RFC 3339). */
  timestamp: string;
  /** The event's ids and fields. */
  data: Record<string, unknown>;
}

/** Request headers, as `fetch` (`Headers`) or Node (`IncomingHttpHeaders`) give them. */
export type WebhookHeaders =
  | Headers
  | Readonly<Record<string, string | readonly string[] | undefined>>;

export interface VerifyWebhookOptions {
  /** How far, in seconds, `webhook-timestamp` may be from now. Default 300. */
  toleranceSeconds?: number;
  /** The current time in milliseconds since the Unix epoch, for tests. Default `Date.now()`. */
  now?: number;
}

const SECRET_PREFIX = "whsec_";
const DEFAULT_TOLERANCE_SECONDS = 300;

/** A header's value, whatever the shape of `headers` and the case of its name. */
const header = (headers: WebhookHeaders, name: string): string | undefined => {
  if (typeof (headers as Headers).get === "function") {
    return (headers as Headers).get(name) ?? undefined;
  }
  const record = headers as Readonly<
    Record<string, string | readonly string[] | undefined>
  >;
  const key = Object.keys(record).find(
    (candidate) => candidate.toLowerCase() === name
  );
  const value = key === undefined ? undefined : record[key];
  return typeof value === "string" ? value : value?.[0];
};

/** The bytes of base64 `text`, or `undefined` when it is not base64. */
const decodeBase64 = (text: string): Uint8Array<ArrayBuffer> | undefined => {
  try {
    const binary = atob(text);
    const bytes = new Uint8Array(binary.length);
    for (let index = 0; index < binary.length; index += 1) {
      bytes[index] = binary.codePointAt(index) ?? 0;
    }
    return bytes;
  } catch {
    return undefined;
  }
};

/** The signed content: `{id}.{timestamp}.` then the body's bytes, as they were received. */
const signedContent = (
  id: string,
  timestamp: string,
  payload: string | Uint8Array
): Uint8Array<ArrayBuffer> => {
  const encoder = new TextEncoder();
  const prefix = encoder.encode(`${id}.${timestamp}.`);
  const body = typeof payload === "string" ? encoder.encode(payload) : payload;
  const content = new Uint8Array(prefix.length + body.length);
  content.set(prefix);
  content.set(body, prefix.length);
  return content;
};

/**
 * Verifies a webhook delivery and returns its body. Norbelys signs every delivery per Standard
 * Webhooks (<https://www.standardwebhooks.com/>): `webhook-signature` holds one `v1,<base64>`
 * HMAC-SHA256 of `{webhook-id}.{webhook-timestamp}.{body}` per signing secret (two during the 24
 * hours after a rotation), and any one of them is enough. Pass the body exactly as received,
 * before any JSON parsing: re-serialized JSON no longer matches its signature.
 *
 * Every attempt of a delivery carries the same `webhook-id` (the delivery's `whd_…` id) and a
 * fresh timestamp and signature, and delivery is at least once: deduplicate on `webhook-id`.
 *
 * ```ts
 * const event = await verifyWebhook(await request.text(), request.headers, secret);
 * if (event.type === "message.sent") {
 *   const message = await norbelys.messages.retrieve(String(event.data.message_id));
 * }
 * ```
 *
 * Uses the platform's Web Crypto (Node 20+, Bun, Deno, browsers, edge runtimes).
 *
 * @param payload - The raw body, as text or bytes.
 * @param headers - The request's headers.
 * @param secret - The endpoint's signing secret (`whsec_…`).
 * @throws {WebhookVerificationError} When a header is missing, the timestamp is outside the
 * tolerance, no signature matches, or the body is not JSON.
 */
export const verifyWebhook = async (
  payload: string | Uint8Array,
  headers: WebhookHeaders,
  secret: string,
  options: VerifyWebhookOptions = {}
): Promise<WebhookEvent> => {
  const id = header(headers, "webhook-id");
  const timestamp = header(headers, "webhook-timestamp");
  const signatures = header(headers, "webhook-signature");
  if (!id || !timestamp || !signatures) {
    throw new WebhookVerificationError(
      "The request lacks a webhook-id, webhook-timestamp or webhook-signature header."
    );
  }
  const seconds = Number(timestamp);
  const now = (options.now ?? Date.now()) / 1000;
  const tolerance = options.toleranceSeconds ?? DEFAULT_TOLERANCE_SECONDS;
  if (!/^\d+$/u.test(timestamp) || Math.abs(now - seconds) > tolerance) {
    throw new WebhookVerificationError(
      "The webhook-timestamp is too old or too new: a replay, or a clock out of sync."
    );
  }
  const key = decodeBase64(
    secret.startsWith(SECRET_PREFIX)
      ? secret.slice(SECRET_PREFIX.length)
      : secret
  );
  if (!key || key.length === 0) {
    throw new WebhookVerificationError(
      "The secret is not a signing secret (`whsec_` and base64)."
    );
  }
  const hmac = await crypto.subtle.importKey(
    "raw",
    key,
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["verify"]
  );
  const content = signedContent(id, timestamp, payload);
  const candidates = signatures
    .split(" ")
    .filter((signature) => signature.startsWith("v1,"))
    .map((signature) => decodeBase64(signature.slice(3)))
    .filter((signature) => signature !== undefined);
  // `verify` compares in constant time; each candidate is one secret's signature.
  const checks = await Promise.all(
    candidates.map((signature) =>
      crypto.subtle.verify("HMAC", hmac, signature, content)
    )
  );
  if (!checks.includes(true)) {
    throw new WebhookVerificationError(
      "No webhook-signature matches the body: check the secret, and pass the raw body."
    );
  }
  const text =
    typeof payload === "string" ? payload : new TextDecoder().decode(payload);
  try {
    return JSON.parse(text) as WebhookEvent;
  } catch (error) {
    throw new WebhookVerificationError("The webhook body is not JSON.", {
      cause: error,
    });
  }
};
