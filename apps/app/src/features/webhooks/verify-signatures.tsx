import { CodeBlock } from "@/components/copy";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";

/** Verifying a delivery with Node's own crypto: no library, and the raw body as it came. */
export const SAMPLE = `import { createHmac, timingSafeEqual } from "node:crypto";

// The endpoint's signing secret: whsec_ then the base64 of 24 to 64 bytes.
const secret = process.env.NORBELYS_WEBHOOK_SECRET ?? "";
const key = Buffer.from(secret.replace(/^whsec_/, ""), "base64");
// Fail closed before the empty string a missing env var becomes a public 0-byte HMAC key.
if (key.length < 24 || key.length > 64) {
  throw new Error(
    "NORBELYS_WEBHOOK_SECRET must be whsec_ plus the base64 of 24 to 64 bytes"
  );
}

/** True when Norbelys signed this raw body less than five minutes ago. */
export function verify(headers: Headers, body: string): boolean {
  const id = headers.get("webhook-id");
  const timestamp = headers.get("webhook-timestamp");
  const signatures = headers.get("webhook-signature");
  if (!id || !timestamp || !signatures) return false;
  if (Math.abs(Date.now() / 1000 - Number(timestamp)) > 300) return false;
  const expected = createHmac("sha256", key)
    .update(\`\${id}.\${timestamp}.\${body}\`)
    .digest();
  // Two signatures, space-separated, for 24 hours after a rotation.
  return signatures.split(" ").some((entry) => {
    const [version, signature = ""] = entry.split(",");
    const given = Buffer.from(signature, "base64");
    return (
      version === "v1" &&
      given.length === expected.length &&
      timingSafeEqual(given, expected)
    );
  });
}`;

const HEADERS = [
  {
    meaning:
      "The delivery's id (whd_…), the same on every attempt: deduplicate on it.",
    name: "webhook-id",
  },
  {
    meaning:
      "The attempt's Unix time in seconds: refuse old ones, so a captured request cannot be replayed.",
    name: "webhook-timestamp",
  },
  {
    meaning:
      "v1, then the base64 HMAC-SHA256 of id.timestamp.body, keyed with the secret's bytes.",
    name: "webhook-signature",
  },
];

/**
 * How a consumer checks that a request came from Norbelys, and how deliveries are retried: the
 * Standard Webhooks headers, a verification in a few lines of Node, and the retry schedule.
 * Any Standard Webhooks library verifies these deliveries as well.
 */
export const VerifySignatures = () => (
  <Card>
    <CardHeader className="flex-col items-start gap-0.5">
      <CardTitle>Verify signatures</CardTitle>
      <CardDescription>
        Each delivery POSTs{" "}
        <code className="text-fg font-mono text-xs">
          {'{"type", "timestamp", "data"}'}
        </code>{" "}
        as JSON with the{" "}
        <a
          className="text-link hover:text-link-hover"
          href="https://www.standardwebhooks.com/"
          rel="noreferrer"
          target="_blank"
        >
          Standard Webhooks
        </a>{" "}
        headers. Verify the raw body before parsing it.
      </CardDescription>
    </CardHeader>
    <CardContent className="flex flex-col gap-4">
      <dl className="flex flex-col gap-2">
        {HEADERS.map((header) => (
          <div
            className="flex flex-col gap-0.5 sm:flex-row sm:gap-4"
            key={header.name}
          >
            <dt className="text-fg w-40 shrink-0 font-mono text-xs font-medium">
              {header.name}
            </dt>
            <dd className="text-fg-2 text-xs">{header.meaning}</dd>
          </div>
        ))}
      </dl>
      <CodeBlock value={SAMPLE} />
      <p className="text-fg-2 text-xs">
        Answer 2xx within 15 seconds. Anything else, a redirect or a timeout is
        retried after 5 s, 5 min, 30 min, 2 h, 5 h, 10 h, 14 h, 20 h and 24 h
        (each wait up to a tenth longer, and a Retry-After on 429, 502, 503 or
        504 is honoured): ten attempts over about three days. A 410 disables the
        endpoint at once; five days without a success disable it too.
      </p>
    </CardContent>
  </Card>
);
