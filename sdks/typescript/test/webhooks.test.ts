import { describe, expect, test } from "bun:test";

import { verifyWebhook, WebhookVerificationError } from "../src/index";

/**
 * The Standard Webhooks reference vector, which the server's signing test pins too: verifying it
 * proves the SDK accepts what Norbelys signs, byte for byte. This is public fixture data:
 * https://github.com/standard-webhooks/standard-webhooks/blob/main/libraries/javascript/src/webhook.test.ts
 */
/** Encodes deliberately public test bytes in the Standard Webhooks wire format. */
const fixtureSecret = (bytes: Uint8Array): string =>
  `whsec_${btoa(String.fromCodePoint(...bytes))}`;

const rotationSecret = fixtureSecret(
  new TextEncoder().encode("second secret of the endpoint")
);

const reference = {
  secret: fixtureSecret(
    new Uint8Array([
      49, 242, 144, 246, 191, 6, 41, 138, 171, 79, 8, 212, 60, 63, 8, 44, 246,
      72, 163, 98, 218, 45, 164, 176,
    ])
  ),
  id: "msg_p5jXN8AQM9LWM0D4loKWxJek",
  timestamp: "1614265330",
  payload: '{"test": 2432232314}',
  signature: "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=",
};
/** The reference vector's own time, in milliseconds. */
const at = Number(reference.timestamp) * 1000;

const headers = (signature = reference.signature): Headers =>
  new Headers({
    "webhook-id": reference.id,
    "webhook-signature": signature,
    "webhook-timestamp": reference.timestamp,
  });

/** HMAC-SHA256 of `{id}.{timestamp}.{payload}` under `secret`, as `v1,<base64>`. */
const sign = async (secret: string, payload: string): Promise<string> => {
  const raw = Uint8Array.from(
    atob(secret.replace(/^whsec_/u, "")),
    (c) => c.codePointAt(0) ?? 0
  );
  const key = await crypto.subtle.importKey(
    "raw",
    raw,
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"]
  );
  const mac = await crypto.subtle.sign(
    "HMAC",
    key,
    new TextEncoder().encode(
      `${reference.id}.${reference.timestamp}.${payload}`
    )
  );
  return `v1,${btoa(String.fromCodePoint(...new Uint8Array(mac)))}`;
};

const failure = async (run: () => Promise<unknown>): Promise<string> => {
  const error = await run().catch((error_: unknown) => error_);
  expect(error).toBeInstanceOf(WebhookVerificationError);
  return (error as WebhookVerificationError).message;
};

describe("verifyWebhook", () => {
  test("accepts the signing key without its optional whsec prefix", async () => {
    const event = await verifyWebhook(
      reference.payload,
      headers(),
      reference.secret.slice(6),
      { now: at }
    );
    expect(event as unknown).toEqual({ test: 2_432_232_314 });
  });
  test("accepts the reference signature and returns the parsed body", async () => {
    const event = await verifyWebhook(
      reference.payload,
      headers(),
      reference.secret,
      { now: at }
    );

    expect(event as unknown).toEqual({ test: 2_432_232_314 });
  });

  test("verifies the raw bytes as well as text", async () => {
    const bytes = new TextEncoder().encode(reference.payload);

    const event = await verifyWebhook(bytes, headers(), reference.secret, {
      now: at,
    });

    expect(event as unknown).toEqual({ test: 2_432_232_314 });
  });

  test("reads Node-style headers of any case, and arrays", async () => {
    const event = await verifyWebhook(
      reference.payload,
      {
        "Webhook-Id": reference.id,
        "webhook-signature": [reference.signature],
        "webhook-timestamp": reference.timestamp,
      },
      reference.secret,
      { now: at }
    );

    expect(event as unknown).toEqual({ test: 2_432_232_314 });
  });

  test("accepts any one of the signatures sent during a secret's rotation", async () => {
    const other = await sign(rotationSecret, reference.payload);

    const event = await verifyWebhook(
      reference.payload,
      headers(`${other} ${reference.signature}`),
      reference.secret,
      { now: at }
    );

    expect(event as unknown).toEqual({ test: 2_432_232_314 });
  });

  test("refuses a body changed after signing, even by whitespace", async () => {
    const message = await failure(() =>
      verifyWebhook('{"test":2432232314}', headers(), reference.secret, {
        now: at,
      })
    );

    expect(message).toContain("No webhook-signature matches");
  });

  test("refuses another secret's signature", async () => {
    const message = await failure(() =>
      verifyWebhook(reference.payload, headers(), rotationSecret, { now: at })
    );

    expect(message).toContain("No webhook-signature matches");
  });

  test("refuses a delivery older or newer than the tolerance, as a replay", async () => {
    for (const now of [at + 301_000, at - 301_000]) {
      // oxlint-disable-next-line no-await-in-loop -- two cases, one after the other
      const message = await failure(() =>
        verifyWebhook(reference.payload, headers(), reference.secret, { now })
      );
      expect(message).toContain("too old or too new");
    }
    await expect(
      verifyWebhook(reference.payload, headers(), reference.secret, {
        now: at + 3_600_000,
        toleranceSeconds: 7200,
      })
    ).resolves.toBeDefined();
  });

  test("refuses a delivery without its headers, or with another scheme's signature", async () => {
    const missing = await failure(() =>
      verifyWebhook(reference.payload, new Headers(), reference.secret, {
        now: at,
      })
    );
    const unversioned = await failure(() =>
      verifyWebhook(
        reference.payload,
        headers(reference.signature.replace("v1,", "v2,")),
        reference.secret,
        { now: at }
      )
    );

    expect(missing).toContain("lacks a webhook-id");
    expect(unversioned).toContain("No webhook-signature matches");
  });

  test("refuses a secret that is not base64", async () => {
    const message = await failure(() =>
      verifyWebhook(reference.payload, headers(), "whsec_!!!", { now: at })
    );

    expect(message).toContain("not a signing secret");
  });

  test("refuses a signed body that is not JSON", async () => {
    const payload = "not json";
    const signature = await sign(reference.secret, payload);

    const message = await failure(() =>
      verifyWebhook(payload, headers(signature), reference.secret, { now: at })
    );

    expect(message).toContain("not JSON");
  });
});
