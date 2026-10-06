import { afterAll, expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

import {
  SAMPLE,
  VerifySignatures,
} from "../src/features/webhooks/verify-signatures";

let counter = 0;
const dir = await mkdtemp(path.join(tmpdir(), "verify-snippet-"));

afterAll(async () => {
  await rm(dir, { recursive: true, force: true });
});

const whsec = (bytes: number, fill = 0x42): string =>
  `whsec_${Buffer.alloc(bytes, fill).toString("base64")}`;

const LOADED = `console.log("LOADED");`;

const signedDriver = (b64: string): string => `
const id = "whd_test";
const ts = Math.floor(Date.now() / 1000);
const body = JSON.stringify({ type: "message.sent", timestamp: ts, data: {} });
const signKey = Buffer.from(${JSON.stringify(b64)}, "base64");
const sig = createHmac("sha256", signKey).update(\`\${id}.\${ts}.\${body}\`).digest("base64");
const headers = new Headers();
headers.set("webhook-id", id);
headers.set("webhook-timestamp", String(ts));
headers.set("webhook-signature", \`v1,\${sig}\`);
console.log(verify(headers, body) ? "ACCEPTED" : "REJECTED");
`;

const forgedDriver = (): string => `
const id = "whd_forged";
const ts = Math.floor(Date.now() / 1000);
const body = JSON.stringify({ type: "suppression.created", timestamp: ts, data: { address: "victim@example.com", reason: "complaint" } });
const signKey = Buffer.alloc(0);
const sig = createHmac("sha256", signKey).update(\`\${id}.\${ts}.\${body}\`).digest("base64");
const headers = new Headers();
headers.set("webhook-id", id);
headers.set("webhook-timestamp", String(ts));
headers.set("webhook-signature", \`v1,\${sig}\`);
console.log(verify(headers, body) ? "ACCEPTED" : "REJECTED");
`;

const staleDriver = (b64: string): string => `
const id = "whd_test";
const ts = Math.floor(Date.now() / 1000) - 600;
const body = JSON.stringify({ type: "message.sent", timestamp: ts, data: {} });
const signKey = Buffer.from(${JSON.stringify(b64)}, "base64");
const sig = createHmac("sha256", signKey).update(\`\${id}.\${ts}.\${body}\`).digest("base64");
const headers = new Headers();
headers.set("webhook-id", id);
headers.set("webhook-timestamp", String(ts));
headers.set("webhook-signature", \`v1,\${sig}\`);
console.log(verify(headers, body) ? "ACCEPTED" : "REJECTED");
`;

const missingDriver = (): string => `
const id = "whd_test";
const ts = Math.floor(Date.now() / 1000);
const body = JSON.stringify({ type: "message.sent", timestamp: ts, data: {} });
const headers = new Headers();
headers.set("webhook-id", id);
headers.set("webhook-timestamp", String(ts));
console.log(verify(headers, body) ? "ACCEPTED" : "REJECTED");
`;

const run = async (
  secret: string | undefined,
  driver: string
): Promise<{ exitCode: number | null; stdout: string; stderr: string }> => {
  const file = path.join(dir, `snippet-${counter}.ts`);
  counter += 1;
  const env = { ...process.env } as Record<string, string | undefined>;
  if (secret === undefined) {
    delete env.NORBELYS_WEBHOOK_SECRET;
  } else {
    env.NORBELYS_WEBHOOK_SECRET = secret;
  }
  await writeFile(file, `${SAMPLE}\n${driver}`, "utf-8");
  const proc = Bun.spawn(["bun", "run", file], {
    env,
    stdout: "pipe",
    stderr: "pipe",
  });
  const [exitCode, stdout, stderr] = await Promise.all([
    proc.exited,
    Bun.readableStreamToText(proc.stdout),
    Bun.readableStreamToText(proc.stderr),
  ]);
  return { exitCode, stdout, stderr };
};

test("the panel still ships the snippet and the component", () => {
  expect(typeof SAMPLE).toBe("string");
  expect(typeof VerifySignatures).toBe("function");
});

test("SAMPLE guards the HMAC key length before any verification", () => {
  expect(SAMPLE).toContain("key.length < 24");
  expect(SAMPLE).toContain("key.length > 64");
  expect(SAMPLE).toContain("throw new Error");
  expect(SAMPLE).toContain("NORBELYS_WEBHOOK_SECRET");
});

test("SAMPLE fail-closes when the secret is unset, empty, or the wrong length", async () => {
  const results = await Promise.all(
    [undefined, "", "whsec_", whsec(23), whsec(65)].map((secret) =>
      run(secret, LOADED)
    )
  );
  for (const result of results) {
    expect(result.exitCode).not.toBe(0);
    expect(result.stderr).toContain("NORBELYS_WEBHOOK_SECRET");
    expect(result.stdout).not.toContain("LOADED");
  }
});

test("SAMPLE loads when the secret decodes to 24 to 64 bytes", async () => {
  const results = await Promise.all(
    [whsec(24), whsec(64)].map((secret) => run(secret, LOADED))
  );
  for (const result of results) {
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toContain("LOADED");
  }
});

test("SAMPLE verifies a delivery signed with the configured secret", async () => {
  const b64 = Buffer.alloc(32, 0x42).toString("base64");
  const result = await run(`whsec_${b64}`, signedDriver(b64));
  expect(result.exitCode).toBe(0);
  expect(result.stdout).toContain("ACCEPTED");
});

test("SAMPLE rejects a signature forged with the public empty key when the secret is configured", async () => {
  const b64 = Buffer.alloc(32, 0x42).toString("base64");
  const result = await run(`whsec_${b64}`, forgedDriver());
  expect(result.exitCode).toBe(0);
  expect(result.stdout).toContain("REJECTED");
});

test("SAMPLE rejects a delivery whose timestamp is outside the five-minute window", async () => {
  const b64 = Buffer.alloc(32, 0x42).toString("base64");
  const result = await run(`whsec_${b64}`, staleDriver(b64));
  expect(result.exitCode).toBe(0);
  expect(result.stdout).toContain("REJECTED");
});

test("SAMPLE rejects a delivery missing a Standard Webhooks header", async () => {
  const b64 = Buffer.alloc(32, 0x42).toString("base64");
  const result = await run(`whsec_${b64}`, missingDriver());
  expect(result.exitCode).toBe(0);
  expect(result.stdout).toContain("REJECTED");
});

test("SAMPLE no longer accepts forged deliveries when NORBELYS_WEBHOOK_SECRET is unset", async () => {
  const result = await run(undefined, forgedDriver());
  expect(result.exitCode).not.toBe(0);
  expect(result.stderr).toContain("NORBELYS_WEBHOOK_SECRET");
  expect(result.stdout).not.toContain("ACCEPTED");
});

test("SAMPLE accepts a delivery when the secret is given without its whsec_ prefix", async () => {
  const b64 = Buffer.alloc(32, 0x42).toString("base64");
  const result = await run(b64, signedDriver(b64));
  expect(result.exitCode).toBe(0);
  expect(result.stdout).toContain("ACCEPTED");
});
