import { describe, expect, test } from "bun:test";

import { APIError, Norbelys, PagePromise } from "../src/index";
import { fakeFetch, lastPage, respond } from "./support";

const page = (
  ids: string[],
  nextCursor: string | null,
  extra: Record<string, unknown> = {}
): Response =>
  respond(200, {
    data: ids.map((id) => ({ id })),
    meta: { has_more: nextCursor !== null, next_cursor: nextCursor, ...extra },
  });

const cursors = (calls: { url: string }[]): (string | null)[] =>
  calls.map((call) => new URL(call.url).searchParams.get("cursor"));

describe("pagination", () => {
  test("iterates every page, one request at a time, keeping the filters", async () => {
    const { fetch, calls } = fakeFetch(
      page(["per_1", "per_2"], "c2", { total: 3 }),
      page(["per_3"], null, { total: 3 })
    );
    const norbelys = new Norbelys({ apiKey: "ak_test", fetch });

    const ids: string[] = [];
    for await (const person of norbelys.people.list({
      limit: 2,
      q: "acme",
    })) {
      ids.push(person.id);
    }

    expect(ids).toEqual(["per_1", "per_2", "per_3"]);
    expect(calls[1]?.url).toBe(
      "https://api.norbelys.com/v1/people?limit=2&q=acme&cursor=c2"
    );
  });

  test("awaiting a list returns its first page", async () => {
    const { fetch } = fakeFetch(page(["grp_1"], null));
    const norbelys = new Norbelys({ apiKey: "ak_test", fetch });

    const first = await norbelys.groups.list();

    expect(first.data.map((group) => group.id)).toEqual(["grp_1"]);
    expect(first.meta.has_more).toBe(false);
    expect(await first.nextPage()).toBeNull();
  });

  test("walks pages by hand with nextPage", async () => {
    const { fetch, calls } = fakeFetch(
      page(["grp_1"], "c2"),
      page(["grp_2"], "c3"),
      page(["grp_3"], null)
    );
    const norbelys = new Norbelys({ apiKey: "ak_test", fetch });

    const first = await norbelys.groups.list({ limit: 1 });
    const second = await first.nextPage();
    const third = await second?.nextPage();

    expect(third?.data.map((group) => group.id)).toEqual(["grp_3"]);
    expect(await third?.nextPage()).toBeNull();
    expect(cursors(calls)).toEqual([null, "c2", "c3"]);
  });

  test("starts at a given cursor, and a page's own options replace the list's", async () => {
    const { fetch, calls } = fakeFetch(
      page(["grp_2"], "c3"),
      page(["grp_3"], null)
    );
    const norbelys = new Norbelys({ apiKey: "ak_test", fetch });
    const first = new AbortController();

    const start = await norbelys.groups.list(
      { cursor: "c2" },
      { signal: first.signal }
    );
    first.abort();
    const next = await start.nextPage({
      signal: new AbortController().signal,
    });

    expect(next?.data.map((group) => group.id)).toEqual(["grp_3"]);
    expect(cursors(calls)).toEqual(["c2", "c3"]);
  });

  test("the list's signal still covers every page of an iteration", async () => {
    const controller = new AbortController();
    const { fetch, calls } = fakeFetch(page(["grp_1"], "c2"));
    const norbelys = new Norbelys({ apiKey: "ak_test", fetch });

    const run = async (): Promise<void> => {
      for await (const group of norbelys.groups.list(
        {},
        { signal: controller.signal }
      )) {
        if (group.id === "grp_1") {
          controller.abort(new Error("user left"));
        }
      }
    };

    await expect(run()).rejects.toThrow("user left");
    expect(calls).toHaveLength(1);
  });

  test("sends one request however often the first page is awaited", async () => {
    const { fetch, calls } = fakeFetch(page(["grp_1"], null));
    const list = new Norbelys({ apiKey: "ak_test", fetch }).groups.list();

    const [one, two] = await Promise.all([list, list]);

    expect(one).toBe(two);
    expect(calls).toHaveLength(1);
  });

  test("sends nothing until the list is awaited or iterated", () => {
    const { fetch, calls } = fakeFetch();

    const list = new Norbelys({ apiKey: "ak_test", fetch }).groups.list();

    expect(list).toBeInstanceOf(PagePromise);
    expect(calls).toHaveLength(0);
  });

  test("stops fetching when the loop breaks", async () => {
    const { fetch, calls } = fakeFetch(page(["grp_1", "grp_2"], "c2"));
    const norbelys = new Norbelys({ apiKey: "ak_test", fetch });

    let seen = 0;
    for await (const group of norbelys.groups.list()) {
      seen += 1;
      if (group.id === "grp_1") {
        break;
      }
    }

    expect(seen).toBe(1);
    expect(calls).toHaveLength(1);
  });

  test("stops at has_more false even when a cursor is present", async () => {
    const { fetch, calls } = fakeFetch(
      respond(200, {
        data: [{ id: "grp_1" }],
        meta: { has_more: false, next_cursor: "c2" },
      })
    );
    const norbelys = new Norbelys({ apiKey: "ak_test", fetch });

    const ids: string[] = [];
    for await (const group of norbelys.groups.list()) {
      ids.push(group.id);
    }

    expect(ids).toEqual(["grp_1"]);
    expect(calls).toHaveLength(1);
  });

  test("rejects the iteration when a later page fails, after yielding the earlier ones", async () => {
    const { fetch } = fakeFetch(
      page(["grp_1"], "c2"),
      respond(404, { code: "NotFound" })
    );
    const norbelys = new Norbelys({ apiKey: "ak_test", fetch });

    const ids: string[] = [];
    const run = async (): Promise<void> => {
      for await (const group of norbelys.groups.list()) {
        ids.push(group.id);
      }
    };

    await expect(run()).rejects.toBeInstanceOf(APIError);
    expect(ids).toEqual(["grp_1"]);
  });

  test("an empty list iterates nothing", async () => {
    const { fetch } = fakeFetch(lastPage());
    const norbelys = new Norbelys({ apiKey: "ak_test", fetch });

    const ids: string[] = [];
    for await (const group of norbelys.groups.list()) {
      ids.push(group.id);
    }

    expect(ids).toEqual([]);
  });
});
