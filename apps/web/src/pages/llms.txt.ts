import type { APIRoute } from "astro";

import { API_URL, MCP_URL, events } from "../developers";
import { APP, DEVELOPERS, DOCS, PAGES, SOURCE } from "../links";
import { getPosts, postUrl } from "../posts";

/**
 * The site for language models (llmstxt.org): what Norbelys is, how a program or an assistant
 * reaches it, and where the details live. Plain Markdown, written for an agent that has been
 * asked to integrate Norbelys or to answer questions about it.
 */
export const GET: APIRoute = async ({ site }) => {
  const base = site ?? new URL("https://norbelys.com");
  const page = (path: string): string => new URL(path, base).href;
  const eventNames = events.flatMap((group) => group.names);
  const posts = await getPosts();
  const text = [
    "# Norbelys",
    "",
    "> Cold email from your own inboxes: personal emails, follow-ups that stop the moment someone replies, and every reply in one inbox. One plan from $20 a month, AI included. The source is Apache-2.0.",
    "",
    "Everything about people, campaigns, messages, replies, sending and webhooks is a public API operation. One OpenAPI contract (99 operations) generates the TypeScript and Python SDKs, the CLI's commands, the MCP server's tools and the API reference.",
    "",
    "## Build on Norbelys",
    "",
    `- [Developers](${page(PAGES.developers)}): how one email travels through Norbelys, the first request, webhooks, MCP, the CLI and self-hosting.`,
    `- [OpenAPI contract](${API_URL}/openapi.json): the whole public API, served without a key. Base URL ${API_URL}, versioned under /v1.`,
    `- [API reference](${DEVELOPERS}): every operation, with examples in cURL, JavaScript and Python.`,
    "- TypeScript SDK: `npm i @norbelys/sdk`, then `new Norbelys()` reads `NORBELYS_API_KEY`.",
    "- Python SDK: `pip install norbelys`, then `Norbelys()` reads `NORBELYS_API_KEY`.",
    `- MCP server: ${MCP_URL}, over streamable HTTP with OAuth 2.1 and PKCE. Its 98 tools are named after the API's operations, such as messages.create and inbound_messages.list, and an assistant sees only the tools its grant allows.`,
    "- CLI: `curl --proto '=https' --tlsv1.2 -LsSf https://cli.norbelys.com/install.sh | sh` on macOS and Linux, then `norbelys login`. Every operation is a command; `norbelys listen --forward-to <url>` forwards your workspace's events to a local server.",
    `- [Source and self-hosting](${SOURCE}): \`bun run selfhost:setup\`, \`bun run selfhost:migrate\`, then \`bun run selfhost:start\`.`,
    "",
    "## Rules the API keeps",
    "",
    "- Authenticate with `Authorization: Bearer <key>`. A key belongs to one workspace; a test key belongs to a test workspace, whose email never reaches a real inbox. There, a recipient whose address starts with bounce, blocked, defer, throttle or uncertain simulates that outcome.",
    "- Every POST that changes something takes an `Idempotency-Key`. The same key within 24 hours replays the first answer (`Idempotent-Replayed: true`) instead of sending a second email. The SDKs generate one for every write.",
    '- Updates take the version you read as `If-Match: "<version>"`; a stale one gets `412 precondition_failed`.',
    "- Errors are RFC 9457 `application/problem+json` with a stable `code` and a `request_id`.",
    "- Lists take `limit` (up to 100) and `cursor`. The SDKs walk every page.",
    "- 6,000 requests a minute per workspace, reported in `RateLimit` headers, with `Retry-After` on a 429.",
    "",
    "## Webhooks",
    "",
    "- Signed the Standard Webhooks way: `webhook-id`, `webhook-timestamp`, `webhook-signature`. Verify with `verifyWebhook` from `@norbelys/sdk`.",
    "- Ten attempts over about three days (now, then 5 s, 5 min, 30 min, 2 h, 5 h, 10 h, 14 h, 20 h, 24 h), 15 seconds each. Answering 410 switches the endpoint off.",
    `- Events: ${eventNames.join(", ")}.`,
    "",
    "## Product",
    "",
    `- [Campaigns](${page(PAGES.campaigns)}): multi-step sequences sent from your own inboxes on a schedule, stopped when someone replies.`,
    `- [AI personalization](${page(PAGES.aiPersonalization)}): a first line for each person from facts Norbelys finds, and replies sorted for you.`,
    `- [Pricing](${page("/#pricing")}): one plan from $20 a month with every feature included.`,
    `- [Sign in or start](${APP})`,
    `- [Documentation](${DOCS})`,
    "",
    "## Optional",
    "",
    ...posts.map(
      (post) =>
        `- [${post.data.title}](${page(postUrl(post))}): ${post.data.description}`
    ),
    "",
  ].join("\n");
  return new Response(text, {
    headers: { "Content-Type": "text/plain; charset=utf-8" },
  });
};
