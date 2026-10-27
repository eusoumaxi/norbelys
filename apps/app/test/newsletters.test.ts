import { expect, test } from "bun:test";

import { Norbelys } from "@norbelys/sdk";

import {
  draftBody,
  draftKey,
  editionName,
  packSchema,
  prepareDrafts,
} from "../src/features/newsletters/model";

const pack = packSchema.parse({
  version: 1,
  id: "review-pack",
  name: "Review pack",
  timezone: "America/Bogota",
  editions: [1, 2].map((number) => ({
    id: `edition-${number}`,
    title: `Edition ${number}`,
    category: "education",
    audience: "Consented subscribers",
    subject: `A useful idea ${number}`,
    preheader: "A short preview",
    html: '<p>Hello</p><a href="{{ unsubscribe_url }}">Unsubscribe</a>',
    source: "https://example.com/blog/idea",
    proposed_at: "2026-11-03T09:00:00-05:00",
  })),
});

test("preparation resumes a partial pack and never enrolls or starts an edition", async () => {
  const first = pack.editions[0];
  const second = pack.editions[1];
  if (!first || !second) {
    throw new Error("The fixture needs two editions");
  }
  const requests: { url: string; method: string }[] = [];
  const api = new Norbelys({
    apiKey: "nb_test_example",
    baseUrl: "https://api.example.com",
    maxRetries: 0,
    fetch: async (url, init) => {
      requests.push({ url, method: init.method ?? "GET" });
      if (init.method === "GET") {
        return Response.json({
          data: [{ name: editionName(pack, first) }],
          meta: { has_more: false },
        });
      }
      expect(JSON.parse(String(init.body))).toEqual(
        draftBody(pack, second, "sid_sender")
      );
      expect(new Headers(init.headers).get("idempotency-key")).toStartWith(
        "newsletter:ws_example:"
      );
      return Response.json({ id: "cmp_draft", status: "draft" });
    },
  });
  const progress: number[] = [];
  await prepareDrafts(api, "ws_example", pack, "sid_sender", (count) =>
    progress.push(count)
  );
  expect(
    requests.map((request) => [new URL(request.url).pathname, request.method])
  ).toEqual([
    ["/v1/campaigns", "GET"],
    ["/v1/campaigns", "POST"],
  ]);
  expect(progress).toEqual([1, 2]);
});

test("a broadcast has one step, no follow-up and stable retry keys scoped to its exact content", async () => {
  const edition = pack.editions[0];
  if (!edition) {
    throw new Error("The fixture needs an edition");
  }
  const body = draftBody(pack, edition, "sid_sender");
  expect(body.steps).toHaveLength(1);
  expect(body.tracking).toEqual({ opens: true, clicks: true });
  expect(body.schedule?.start_at).toBe(edition.proposed_at);
  expect(await draftKey("ws_a", body)).toBe(await draftKey("ws_a", body));
  expect(await draftKey("ws_a", body)).not.toBe(await draftKey("ws_b", body));
  expect(await draftKey("ws_a", body)).not.toBe(
    await draftKey("ws_a", { ...body, name: "Changed" })
  );
});

test("invalid packs cannot hide duplicate editions or an unsafe editorial source", () => {
  expect(
    packSchema.safeParse({
      ...pack,
      editions: [pack.editions[0], pack.editions[0]],
    }).success
  ).toBe(false);
  expect(
    packSchema.safeParse({ ...pack, timezone: "invented/timezone" }).success
  ).toBe(false);
  expect(
    packSchema.safeParse({
      ...pack,
      editions: [{ ...pack.editions[0], source: "javascript:alert(1)" }],
    }).success
  ).toBe(false);
});
