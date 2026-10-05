/**
 * The developers page, as data: the code it shows, the API's habits, the webhook schedule and
 * events, the MCP server's clients and the CLI. Every endpoint, header, number and command here
 * comes from the repository (the OpenAPI contract, the SDKs, the CLI and the server); the people
 * and addresses in the samples are made up.
 */

/** A code sample: what its tab says, the highlighter's language and the code. */
export interface Sample {
  readonly label: string;
  readonly lang: "bash" | "json" | "python" | "typescript" | "http";
  readonly code: string;
}

/** The hosted MCP server, as the dashboard's Connect panel gives it. */
export const MCP_URL = "https://mcp.norbelys.com/mcp";
/** The API's public origin; its contract is served unauthenticated at `/openapi.json`. */
export const API_URL = "https://api.norbelys.com";

/** The first request: one email, in each language. */
export const send: readonly Sample[] = [
  {
    code: `import { Norbelys } from "@norbelys/sdk";

const norbelys = new Norbelys(); // reads NORBELYS_API_KEY

const message = await norbelys.messages.create({
  from: "sam@acme.example",
  to: ["alex@northwind.example"],
  subject: "Quick idea for {{ variables.company }}",
  html: "<p>Hi {{ variables.name }}, …</p>",
  variables: { name: "Alex", company: "Northwind" },
});`,
    label: "TypeScript",
    lang: "typescript",
  },
  {
    code: `from norbelys import Norbelys

with Norbelys() as norbelys:  # reads NORBELYS_API_KEY
    message = norbelys.messages.create({
        "from": "sam@acme.example",
        "to": ["alex@northwind.example"],
        "subject": "Quick idea for {{ variables.company }}",
        "html": "<p>Hi {{ variables.name }}, …</p>",
        "variables": {"name": "Alex", "company": "Northwind"},
    })`,
    label: "Python",
    lang: "python",
  },
  {
    code: `curl https://api.norbelys.com/v1/messages \\
  -H "Authorization: Bearer $NORBELYS_API_KEY" \\
  -H "Idempotency-Key: first-email-alex" \\
  -H "Content-Type: application/json" \\
  -d '{
    "from": "sam@acme.example",
    "to": ["alex@northwind.example"],
    "subject": "Quick idea for Northwind",
    "html": "<p>Hi Alex, …</p>"
  }'`,
    label: "cURL",
    lang: "bash",
  },
  {
    code: `norbelys messages create -d '{
  "from": "sam@acme.example",
  "to": ["alex@northwind.example"],
  "subject": "Quick idea for Northwind",
  "html": "<p>Hi Alex, …</p>"
}'`,
    label: "CLI",
    lang: "bash",
  },
];

/** What the first request answers: queued, with the ids to follow it by. */
export const accepted = `{
  "id": "msg_01J9…",
  "kind": "direct",
  "state": "queued",
  "thread_id": "thr_01J9…",
  "subject": "Quick idea for Northwind",
  …
}`;

/** How to install each way in. */
export const install = [
  { command: "npm i @norbelys/sdk", label: "TypeScript" },
  { command: "pip install norbelys", label: "Python" },
  {
    command:
      "curl --proto '=https' --tlsv1.2 -LsSf https://cli.norbelys.com/install.sh | sh",
    label: "CLI",
  },
] as const;

/**
 * Test mode: a test workspace's messages never reach a provider. The first word of each
 * recipient's local part decides what the fake transport answers.
 */
export const testAddresses = [
  { address: "bounce@", outcome: "Refused at RCPT TO", code: "550 5.1.1" },
  { address: "blocked@", outcome: "Refused for good", code: "550 5.7.1" },
  {
    address: "defer@",
    outcome: "Refused for now, tried again",
    code: "451 4.3.0",
  },
  { address: "throttle@", outcome: "Asked to slow down", code: "421 4.7.0" },
  {
    address: "uncertain@",
    outcome: "Lost reply, never resent",
    code: "uncertain",
  },
  { address: "anything else", outcome: "Accepted", code: "250" },
] as const;

/**
 * One request, line by line, and why each line is there. `note` lines carry the explanation; the
 * rest are the plain HTTP around them.
 */
export interface ExchangeLine {
  readonly text: string;
  readonly kind?: "start" | "header" | "body";
  readonly note?: { readonly name: string; readonly text: string };
}

export const retry: readonly ExchangeLine[] = [
  { kind: "start", text: "POST /v1/messages HTTP/1.1" },
  {
    kind: "header",
    note: {
      name: "Keys",
      text: "A test key belongs to a test workspace, where nothing reaches an inbox. Keys are shown once and stored only as a hash.",
    },
    text: "Authorization: Bearer nb_test_…",
  },
  {
    kind: "header",
    note: {
      name: "Retries",
      text: "Send the same key again within 24 hours and you get the first answer back. The SDKs make one up for every write.",
    },
    text: "Idempotency-Key: first-email-alex",
  },
  {
    kind: "body",
    text: '{"from": "sam@acme.example", "to": ["alex@northwind.example"], …}',
  },
];

export const replayed: readonly ExchangeLine[] = [
  { kind: "start", text: "HTTP/1.1 202 Accepted" },
  {
    kind: "header",
    note: {
      name: "No second email",
      text: "The answer to the first request, replayed. Nothing new was queued.",
    },
    text: "Idempotent-Replayed: true",
  },
  { kind: "header", text: "Location: /v1/messages/msg_01J9…" },
  {
    kind: "header",
    note: {
      name: "Limits",
      text: "6,000 requests a minute per workspace, reported as you go, with Retry-After when you reach it.",
    },
    text: 'RateLimit: "default";r=5998;t=1',
  },
];

export const conflict: readonly ExchangeLine[] = [
  { kind: "start", text: "PATCH /v1/campaigns/cmp_01J9… HTTP/1.1" },
  {
    kind: "header",
    note: {
      name: "Updates",
      text: "Send the version you read. If someone changed it since, nothing is overwritten.",
    },
    text: 'If-Match: "1790000000000000"',
  },
];

export const problem: readonly ExchangeLine[] = [
  { kind: "start", text: "HTTP/1.1 412 Precondition Failed" },
  {
    kind: "header",
    note: {
      name: "Errors",
      text: "Problem details (RFC 9457) with a stable code to switch on, and a request id to quote to support.",
    },
    text: "Content-Type: application/problem+json",
  },
  {
    kind: "body",
    text: '{"code": "precondition_failed", "request_id": "0199b8a2-…", …}',
  },
];

/**
 * When a failed delivery is tried again: the wait before each of the ten attempts, and roughly
 * how long after the event each one comes (75 h 35 min 5 s in all, before jitter).
 */
export const attempts = [
  { elapsed: "0 s", wait: "now" },
  { elapsed: "5 s", wait: "5 s" },
  { elapsed: "5 min", wait: "5 min" },
  { elapsed: "35 min", wait: "30 min" },
  { elapsed: "2 h 35 min", wait: "2 h" },
  { elapsed: "7 h 35 min", wait: "5 h" },
  { elapsed: "17 h 35 min", wait: "10 h" },
  { elapsed: "1 d 7 h", wait: "14 h" },
  { elapsed: "2 d 3 h", wait: "20 h" },
  { elapsed: "3 d 3 h", wait: "24 h" },
] as const;

/** Every event a webhook endpoint can subscribe to, grouped by what it's about. */
export const events = [
  {
    group: "Messages",
    names: [
      "message.queued",
      "message.sent",
      "message.failed",
      "message.uncertain",
      "message.cancelled",
      "message.snippets_fallback",
    ],
  },
  {
    group: "Delivery and replies",
    names: ["delivery_event.recorded", "inbound_message.received"],
  },
  {
    group: "Campaigns",
    names: [
      "enrollment.stopped",
      "enrollment.completed",
      "campaign.status_changed",
    ],
  },
  { group: "Mailboxes", names: ["connection.health_changed"] },
  {
    group: "Lists",
    names: ["import.completed", "export.completed", "suppression.created"],
  },
  { group: "AI", names: ["ai.budget_warning", "ai.budget_exceeded"] },
  { group: "Endpoints", names: ["endpoint.test", "webhook_endpoint.disabled"] },
] as const;

export const verify: readonly Sample[] = [
  {
    code: `import { verifyWebhook } from "@norbelys/sdk";

export async function POST(request: Request) {
  const event = await verifyWebhook(
    await request.text(),
    request.headers,
    process.env.NORBELYS_WEBHOOK_SECRET!, // whsec_…
  );

  if (event.type === "inbound_message.received") {
    // Fetch the reply by its id and hand it to your CRM.
  }
  return new Response(null, { status: 204 });
}`,
    label: "TypeScript",
    lang: "typescript",
  },
];

/** Where an assistant connects, client by client. */
export const assistants: readonly Sample[] = [
  {
    code: `claude mcp add --transport http \\
  norbelys ${MCP_URL}`,
    label: "Claude Code",
    lang: "bash",
  },
  {
    code: `// .cursor/mcp.json
{
  "mcpServers": {
    "norbelys": { "url": "${MCP_URL}" }
  }
}`,
    label: "Cursor",
    lang: "json",
  },
  {
    code: `// .vscode/mcp.json
{
  "servers": {
    "norbelys": { "type": "http", "url": "${MCP_URL}" }
  }
}`,
    label: "VS Code",
    lang: "json",
  },
  {
    code: `# Claude, ChatGPT or any client that takes a remote MCP server:
# add a custom connector with this address.
${MCP_URL}`,
    label: "Any client",
    lang: "bash",
  },
];

/** What a coding agent should know before it writes a line of the integration. */
export const agentPrompt = `Add Norbelys (cold email from my own inboxes) to this project.

- Install the SDK: npm i @norbelys/sdk (TypeScript) or pip install norbelys (Python).
- Read the API key from NORBELYS_API_KEY; never put it in code. Use a test-mode key while building: nothing reaches an inbox, and a recipient address starting with bounce, blocked, defer, throttle or uncertain simulates that outcome.
- Send with norbelys.messages.create({ from, to, subject, html, variables }). It answers 202 with the message in state "queued".
- Every write takes an Idempotency-Key. The SDK generates one; pass your own idempotencyKey for an action you may repeat after a crash, and reuse it with the same body.
- Errors are RFC 9457 problem+json: switch on "code", log "request_id".
- Verify webhooks with verifyWebhook(body, headers, secret) from @norbelys/sdk before trusting them. Answer 2xx quickly; Norbelys retries ten times over about three days.
- The whole API, 99 operations: ${API_URL}/openapi.json`;

/** The CLI's everyday commands, as a session would run them. */
export const cli = `# Approve this device in your browser, once
norbelys login

norbelys campaigns list --limit 20
norbelys people create --email alex@northwind.example

# Your workspace's events, on your laptop
norbelys listen --forward-to localhost:3000/hooks
norbelys trigger message.sent`;

/** Running Norbelys yourself: the three steps, in order. */
export const selfhost = [
  {
    command: "bun run selfhost:setup",
    text: "Writes private settings, pulls the pinned images and starts Postgres.",
  },
  {
    command: "bun run selfhost:migrate",
    text: "Prepares the database from a checkout of the same release.",
  },
  {
    command: "bun run selfhost:start",
    text: "Checks the schema and starts every service. The dashboard opens on localhost:5173.",
  },
] as const;

/** One process `bun run selfhost:start` runs, as `compose.yml` names it. */
export interface Service {
  readonly name: string;
  /** Where it sits on the drawing: a three-by-three grid, with Postgres in the middle. */
  readonly column: 1 | 2 | 3;
  readonly row: 1 | 2 | 3;
  /** The port it publishes on 127.0.0.1, if it publishes one. */
  readonly port?: string;
  /** What it talks to: Postgres, or for the dashboard, only the API. */
  readonly to?: string;
}

/** What runs, laid out the way the map lays it out, with the dashboard (`app`) on top. */
export const services: readonly Service[] = [
  { column: 2, name: "postgres", port: "5432", row: 2 },
  { column: 1, name: "api", port: "3001", row: 1, to: "postgres" },
  { column: 1, name: "worker", row: 3, to: "postgres" },
  { column: 3, name: "sender", row: 3, to: "postgres" },
  { column: 3, name: "inbox", row: 1, to: "postgres" },
  { column: 2, name: "tracking", port: "3002", row: 3, to: "postgres" },
  { column: 2, name: "app", port: "5173", row: 1, to: "api" },
];
