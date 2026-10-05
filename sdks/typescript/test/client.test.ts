import { describe, expect, test } from "bun:test";

import { Core, ifMatchHeader } from "../src/core";
import { APIError, Norbelys, NorbelysError } from "../src/index";
import {
  fakeFetch,
  header,
  inBrowser,
  jsonBody,
  respond,
  withEnv,
} from "./support";

describe("credentials", () => {
  test("sends the API key as a bearer token to the contract's URL", async () => {
    const { fetch, calls } = fakeFetch(respond(200, { id: "cmp_1" }));
    const norbelys = new Norbelys({ apiKey: "ak_test", fetch });

    const campaign = await norbelys.campaigns.retrieve("cmp_1");

    expect(campaign.id).toBe("cmp_1");
    expect(calls[0]?.url).toBe("https://api.norbelys.com/v1/campaigns/cmp_1");
    expect(header(calls[0], "authorization")).toBe("Bearer ak_test");
  });

  test("asks the token callback before every request, against any base URL", async () => {
    let issued = 0;
    const { fetch, calls } = fakeFetch(respond(204), respond(204));
    const norbelys = new Norbelys({
      baseUrl: "https://norbelys.example.org/",
      fetch,
      token: () => {
        issued += 1;
        return `session_${issued}`;
      },
    });

    await norbelys.groups.delete("grp_1");
    await norbelys.groups.delete("grp_2");

    expect(calls.map((call) => header(call, "authorization"))).toEqual([
      "Bearer session_1",
      "Bearer session_2",
    ]);
    expect(calls[0]?.url).toBe("https://norbelys.example.org/v1/groups/grp_1");
  });

  test("awaits an asynchronous token callback", async () => {
    const { fetch, calls } = fakeFetch(respond(204));
    const norbelys = new Norbelys({
      fetch,
      token: () => Promise.resolve("session_async"),
    });

    await norbelys.groups.delete("grp_1");

    expect(header(calls[0], "authorization")).toBe("Bearer session_async");
  });

  test("prefers the apiKey option to a token callback", async () => {
    let asked = false;
    const { fetch, calls } = fakeFetch(respond(204));
    const norbelys = new Norbelys({
      apiKey: "ak_test",
      fetch,
      token: () => {
        asked = true;
        return "session";
      },
    });

    await norbelys.groups.delete("grp_1");

    expect(header(calls[0], "authorization")).toBe("Bearer ak_test");
    expect(asked).toBe(false);
  });

  test("falls back to NORBELYS_API_KEY on servers", async () => {
    await withEnv("NORBELYS_API_KEY", "ak_env", async () => {
      const { fetch, calls } = fakeFetch(respond(204));

      await new Norbelys({ fetch }).groups.delete("grp_1");

      expect(header(calls[0], "authorization")).toBe("Bearer ak_env");
    });
  });

  test("uses a token callback before NORBELYS_API_KEY", async () => {
    await withEnv("NORBELYS_API_KEY", "ak_env", async () => {
      const { fetch, calls } = fakeFetch(respond(204));

      await new Norbelys({ fetch, token: () => "session" }).groups.delete(
        "grp_1"
      );

      expect(header(calls[0], "authorization")).toBe("Bearer session");
    });
  });

  test("sends no authorization when the token callback has none", async () => {
    const { fetch, calls } = fakeFetch(respond(401, { code: "Unauthorized" }));
    const norbelys = new Norbelys({ fetch, token: () => null });

    await expect(norbelys.groups.retrieve("grp_1")).rejects.toMatchObject({
      code: "Unauthorized",
      status: 401,
    });
    expect(header(calls[0], "authorization")).toBeNull();
    expect(calls).toHaveLength(1);
  });

  test("rethrows a failing token callback without sending or retrying", async () => {
    let asked = 0;
    const { fetch, calls } = fakeFetch();
    const norbelys = new Norbelys({
      fetch,
      token: () => {
        asked += 1;
        throw new Error("signed out");
      },
    });

    await expect(norbelys.groups.retrieve("grp_1")).rejects.toThrow(
      "signed out"
    );
    expect(asked).toBe(1);
    expect(calls).toHaveLength(0);
  });

  test("refuses an API key in a browser unless explicitly allowed", () => {
    inBrowser(() => {
      expect(() => new Norbelys({ apiKey: "ak_test" })).toThrow(NorbelysError);
      expect(
        () => new Norbelys({ apiKey: "ak_test", dangerouslyAllowBrowser: true })
      ).not.toThrow();
    });
  });

  test("refuses NORBELYS_API_KEY in a browser too", async () => {
    await withEnv("NORBELYS_API_KEY", "ak_env", () => {
      inBrowser(() => {
        expect(() => new Norbelys()).toThrow("must not run in a browser");
      });
    });
  });

  test("accepts a token callback in a browser", () => {
    inBrowser(() => {
      expect(() => new Norbelys({ token: () => "session" })).not.toThrow();
    });
  });

  test("requires a credential", async () => {
    await withEnv("NORBELYS_API_KEY", undefined, () => {
      expect(() => new Norbelys()).toThrow("Missing credentials");
    });
  });
});

describe("base URL", () => {
  test("defaults to the hosted API", async () => {
    const { fetch, calls } = fakeFetch(respond(204));

    await new Norbelys({ apiKey: "ak_test", fetch }).groups.delete("grp_1");

    expect(calls[0]?.url).toBe("https://api.norbelys.com/v1/groups/grp_1");
  });

  test("resolves a relative base URL against the page, for a same-origin proxy", async () => {
    const { fetch, calls } = fakeFetch(respond(204));
    const norbelys = inBrowser(
      () => new Norbelys({ baseUrl: "/api/", fetch, token: () => "session" })
    );

    await norbelys.groups.delete("grp_1");

    expect(calls[0]?.url).toBe("https://app.example.com/api/v1/groups/grp_1");
  });

  test("rejects a relative base URL outside a browser", () => {
    expect(() => new Norbelys({ apiKey: "ak_test", baseUrl: "/api" })).toThrow(
      "needs a browser location"
    );
  });
});

describe("requests", () => {
  test("merges client and per-request headers but keeps its own", async () => {
    const { fetch, calls } = fakeFetch(respond(200, { id: "grp_1" }));
    const norbelys = new Norbelys({
      apiKey: "ak_test",
      fetch,
      headers: { "x-team": "growth", "x-trace": "client" },
    });

    await norbelys.groups.retrieve("grp_1", {
      headers: {
        accept: "text/html",
        authorization: "Bearer forged",
        "x-trace": "request",
      },
    });

    expect(header(calls[0], "x-team")).toBe("growth");
    expect(header(calls[0], "x-trace")).toBe("request");
    expect(header(calls[0], "authorization")).toBe("Bearer ak_test");
    expect(header(calls[0], "accept")).toBe("application/json");
  });

  test("sends a JSON body with its content type, and no body on reads", async () => {
    const { fetch, calls } = fakeFetch(
      respond(201, { id: "grp_1" }),
      respond(200, { id: "grp_1" })
    );
    const norbelys = new Norbelys({ apiKey: "ak_test", fetch });

    await norbelys.groups.create({ description: null, name: "VIP" });
    await norbelys.groups.retrieve("grp_1");

    expect(calls[0]?.init.method).toBe("POST");
    expect(header(calls[0], "content-type")).toBe("application/json");
    expect(jsonBody(calls[0])).toEqual({ description: null, name: "VIP" });
    expect(calls[1]?.init.method).toBe("GET");
    expect(calls[1]?.init.body).toBeUndefined();
    expect(header(calls[1], "content-type")).toBeNull();
  });

  test("encodes path ids so they cannot change the route", async () => {
    const { fetch, calls } = fakeFetch(respond(200, {}));

    await new Norbelys({ apiKey: "ak_test", fetch }).groups.retrieve(
      "grp 1/../x?y"
    );

    expect(calls[0]?.url).toBe(
      "https://api.norbelys.com/v1/groups/grp%201%2F..%2Fx%3Fy"
    );
  });

  test("rejects an empty path id before sending", async () => {
    const { fetch, calls } = fakeFetch();

    await expect(
      new Norbelys({ apiKey: "ak_test", fetch }).groups.retrieve("")
    ).rejects.toThrow("Missing path parameter {id}");
    expect(calls).toHaveLength(0);
  });

  test("encodes a query: repeated arrays, JSON objects, plain scalars, no empty values", async () => {
    const { fetch, calls } = fakeFetch(respond(200, {}));
    const core = new Core({ apiKey: "ak_test", fetch });

    await core.request(
      { idempotent: false, method: "GET", path: "/v1/things" },
      [],
      {
        query: {
          absent: undefined,
          filter: { match: "All" },
          flag: false,
          none: null,
          size: 0,
          tag: ["a", "b"],
        },
      }
    );

    expect(new URL(calls[0]?.url ?? "").search).toBe(
      `?filter=${encodeURIComponent('{"match":"All"}')}&flag=false&size=0&tag=a&tag=b`
    );
  });

  test("resolves a 204 answer to undefined", async () => {
    const { fetch } = fakeFetch(respond(204));

    const result = await new Norbelys({
      apiKey: "ak_test",
      fetch,
    }).groups.delete("grp_1");

    expect(result).toBeUndefined();
  });

  test("returns a successful non-JSON body as text", async () => {
    const { fetch } = fakeFetch(
      new Response("pong", { headers: { "content-type": "text/plain" } })
    );
    const core = new Core({ apiKey: "ak_test", fetch });

    const result = await core.request(
      { idempotent: false, method: "GET", path: "/v1/ping" },
      [],
      {}
    );

    expect(result).toBe("pong");
  });

  test("passes each attempt its own abort signal", async () => {
    const { fetch, calls } = fakeFetch(respond(200, { id: "grp_1" }));

    await new Norbelys({ apiKey: "ak_test", fetch }).groups.retrieve("grp_1");

    expect(calls[0]?.init.signal).toBeInstanceOf(AbortSignal);
    expect(calls[0]?.init.signal?.aborted).toBe(false);
  });

  test("sends an update's version as a quoted If-Match, over a header of the same name", async () => {
    const { fetch, calls } = fakeFetch(respond(200, { id: "grp_1" }));

    await new Norbelys({ apiKey: "ak_test", fetch }).groups.update(
      "grp_1",
      { name: "VIP" },
      { headers: { "If-Match": '"1"' }, ifMatch: 1_790_000_000_000_000 }
    );

    expect(header(calls[0], "if-match")).toBe('"1790000000000000"');
  });

  test.each([
    [1_790_000_000_000_000, '"1790000000000000"'],
    ["1790000000000000", '"1790000000000000"'],
    ['"1790000000000000"', '"1790000000000000"'],
    ['W/"1"', 'W/"1"'],
    ["*", "*"],
  ])("writes If-Match for %p as %p", (version, value) => {
    expect(ifMatchHeader(version)).toBe(value);
  });

  test("reports an error answer as APIError, never as a value", async () => {
    const { fetch } = fakeFetch(respond(404, { code: "NotFound" }));

    await expect(
      new Norbelys({ apiKey: "ak_test", fetch }).groups.retrieve("grp_1")
    ).rejects.toBeInstanceOf(APIError);
  });
});
