// oxlint-disable require-await, no-await-expression-member -- Async storage mocks and direct response assertions keep the wire tests readable.
import { afterEach, expect, spyOn, test } from "bun:test";

import { download, objectKey, precondition, validRange } from "./index";

const data = new TextEncoder().encode("0123456789");
const metadata = () => ({
  size: 10,
  httpEtag: '"sha"',
  uploaded: new Date("2026-10-04T12:00:00Z"),
  writeHttpMetadata: (h: Headers) =>
    h.set("content-type", "application/octet-stream"),
});
let calls = 0;
const bucket = (missing = false, failing = false) => ({
  async head() {
    calls += 1;
    if (failing) {
      throw new Error("unavailable");
    }
    return missing ? null : metadata();
  },
  async get(_key: string, options?: { range?: Headers }) {
    calls += 1;
    if (failing) {
      throw new Error("unavailable");
    }
    if (missing) {
      return null;
    }
    const range = options?.range?.get("range");
    return {
      ...metadata(),
      body: range ? data.slice(2, 5) : data,
      range: range ? { offset: 2, length: 3 } : undefined,
    };
  },
});
const context = {
  waitUntil: (_promise: Promise<unknown>) => {},
};
const request = (
  path = "/releases/norbelys-cli-v0.1.0/cli.tar.xz",
  headers?: HeadersInit,
  method = "GET"
) => new Request(`https://cli.norbelys.com${path}`, { headers, method });
afterEach(() => {
  calls = 0;
});
test("only known promotion files and versioned assets are public", () => {
  for (const path of [
    "/latest.json",
    "/install.sh",
    "/install.ps1",
    "/releases/norbelys-cli-v1.2.3-beta.1/cli.tar.xz",
  ]) {
    expect(objectKey(path)).toBeDefined();
  }
  for (const path of [
    "/bucket",
    "/private/token",
    "/releases/latest/foo",
    "/releases/norbelys-cli-v1.2.3/a/b",
    "/releases/norbelys-cli-v1.2.3/%2fenv",
  ]) {
    expect(objectKey(path)).toBeUndefined();
  }
});
test("the homepage needs no bucket reads and writes are refused", async () => {
  const env = { RELEASES: bucket() };
  expect(await (await download(request("/"), env, context)).text()).toContain(
    "cli.norbelys.com/install.sh"
  );
  expect(
    await (await download(request("/", undefined, "HEAD"), env, context)).text()
  ).toBe("");
  expect(
    (await download(request("/", undefined, "POST"), env, context)).status
  ).toBe(405);
  expect((await download(request("/private"), env, context)).status).toBe(404);
  expect(calls).toBe(0);
});
test("downloads stream bytes and HEAD keeps their size", async () => {
  const env = { RELEASES: bucket() };
  const result = await download(request(), env, context);
  expect(await result.text()).toBe("0123456789");
  expect(result.headers.get("cache-control")).toContain("immutable");
  expect(result.headers.get("etag")).toBe('"sha"');
  const head = await download(
    request(undefined, undefined, "HEAD"),
    env,
    context
  );
  expect(head.headers.get("content-length")).toBe("10");
  expect(await head.text()).toBe("");
  expect(
    (await download(request("/install.sh"), env, context)).headers.get(
      "cache-control"
    )
  ).toContain("must-revalidate");
});
test("ranges resume downloads and respect If-Range", async () => {
  const env = { RELEASES: bucket() };
  const result = await download(
    request(undefined, { range: "bytes=2-4" }),
    env,
    context
  );
  expect(result.status).toBe(206);
  expect(await result.text()).toBe("234");
  expect(result.headers.get("content-range")).toBe("bytes 2-4/10");
  expect(
    (await download(request(undefined, { range: "bytes=12-" }), env, context))
      .status
  ).toBe(416);
  expect(
    (
      await download(
        request(undefined, { range: "bytes=2-4", "if-range": '"other"' }),
        env,
        context
      )
    ).status
  ).toBe(200);
  expect(
    (
      await download(
        request(undefined, { range: "bytes=2-4", "if-range": '"sha"' }),
        env,
        context
      )
    ).status
  ).toBe(206);
});
test("validators distinguish weak reads, strong writes and date precedence", async () => {
  const env = { RELEASES: bucket() };
  expect(
    (
      await download(
        request(undefined, { "if-none-match": 'W/"sha"' }, "HEAD"),
        env,
        context
      )
    ).status
  ).toBe(304);
  expect(
    (
      await download(
        request(undefined, { "if-match": '"other"', "if-none-match": '"sha"' }),
        env,
        context
      )
    ).status
  ).toBe(412);
  const object = metadata();
  expect(
    precondition(
      new Headers({
        "if-match": "*",
        "if-unmodified-since": "Sat, 01 Jan 2000 00:00:00 GMT",
      }),
      object
    )
  ).toBeUndefined();
  expect(
    precondition(
      new Headers({ "if-unmodified-since": "Sat, 01 Jan 2000 00:00:00 GMT" }),
      object
    )
  ).toBe(412);
  expect(
    precondition(
      new Headers({ "if-modified-since": object.uploaded.toUTCString() }),
      object
    )
  ).toBe(304);
  expect(
    precondition(
      new Headers({
        "if-none-match": '"other"',
        "if-modified-since": object.uploaded.toUTCString(),
      }),
      object
    )
  ).toBeUndefined();
});
test("missing objects and storage failures cannot be cached as releases", async () => {
  expect(
    (await download(request(), { RELEASES: bucket(true) }, context)).status
  ).toBe(404);
  expect(
    (
      await download(
        request(undefined, { range: "bytes=2-4" }),
        { RELEASES: bucket(true) },
        context
      )
    ).status
  ).toBe(404);
  expect(
    (
      await download(
        request(undefined, { range: "bytes=2-4" }),
        { RELEASES: bucket(false, true) },
        context
      )
    ).status
  ).toBe(503);
});
test("range parsing rejects empty, reversed and multiple ranges", () => {
  for (const value of ["bytes=-3", "bytes=2-", "bytes=2-9"]) {
    expect(validRange(value, 10)).toBe(true);
  }
  for (const value of [
    "bytes=-",
    "bytes=-0",
    "bytes=9-2",
    "bytes=1-2,3-4",
    "bytes=10-",
  ]) {
    expect(validRange(value, 10)).toBe(false);
  }
  expect(validRange("bytes=0-", 0)).toBe(false);
});
test("edge cache reuses full immutable objects without an R2 read", async () => {
  const previous = Object.getOwnPropertyDescriptor(globalThis, "caches");
  const put = [] as string[];
  Object.defineProperty(globalThis, "caches", {
    configurable: true,
    value: {
      default: {
        match: async () => new Response("cached"),
        put: async (r: Request) => {
          put.push(r.url);
        },
      },
    },
  });
  try {
    expect(
      await (await download(request(), { RELEASES: bucket() }, context)).text()
    ).toBe("cached");
    expect(calls).toBe(0);
    Object.defineProperty(globalThis, "caches", {
      configurable: true,
      value: {
        default: {
          match: async () => {},
          put: async (r: Request) => {
            put.push(r.url);
          },
        },
      },
    });
    await download(request(), { RELEASES: bucket() }, context);
    expect(put).toEqual([
      "https://cli.norbelys.com/releases/norbelys-cli-v0.1.0/cli.tar.xz",
    ]);
  } finally {
    if (previous) {
      Object.defineProperty(globalThis, "caches", previous);
    } else {
      Reflect.deleteProperty(globalThis, "caches");
    }
  }
});
test("missing R2 content metadata falls back to the asset media type", async () => {
  const store = {
    head: () => Promise.resolve(metadata()),
    get: () =>
      Promise.resolve({
        ...metadata(),
        writeHttpMetadata: () => {},
        body: data,
      }),
  };
  await Promise.all(
    (
      [
        ["/latest.json", "application/json"],
        ["/install.sh", "text/plain; charset=utf-8"],
        ["/releases/norbelys-cli-v0.1.0/cli.zip", "application/octet-stream"],
      ] as const
    ).map(async ([path, media]) => {
      const response = await download(
        request(path),
        { RELEASES: store },
        context
      );
      expect(response.headers.get("content-type")).toBe(media);
    })
  );
});

test("an R2 conditional read protects against object changes after HEAD", async () => {
  const store = {
    head: () => Promise.resolve(metadata()),
    get: () => Promise.resolve(metadata()),
  };
  const env = { RELEASES: store };
  const changed = await download(
    request(undefined, { "if-match": '"sha"' }),
    env,
    context
  );
  expect(changed.status).toBe(412);
  expect(await changed.text()).toBe("");
  const current = await download(
    request(undefined, { "if-none-match": '"sha"' }),
    env,
    context
  );
  expect(current.status).toBe(304);
});

test("suffix ranges preserve the last bytes and the complete-object size", async () => {
  const store = {
    head: () => Promise.resolve(metadata()),
    get: () =>
      Promise.resolve({
        ...metadata(),
        body: data.slice(7),
        range: { suffix: 3 },
      }),
  };
  const response = await download(
    request(undefined, { range: "bytes=-3" }),
    { RELEASES: store },
    context
  );
  expect(response.status).toBe(206);
  expect(response.headers.get("content-range")).toBe("bytes 7-9/10");
  expect(response.headers.get("content-length")).toBe("3");
  expect(await response.text()).toBe("789");
});

test("cache failures fall back to R2 and produce finite diagnostics", async () => {
  const previous = Object.getOwnPropertyDescriptor(globalThis, "caches");
  const diagnostics: string[] = [];
  const output = spyOn(console, "error").mockImplementation((line: unknown) => {
    diagnostics.push(String(line));
  });
  const writes: Promise<unknown>[] = [];
  Object.defineProperty(globalThis, "caches", {
    configurable: true,
    value: {
      default: {
        match: () => Promise.reject(new Error("secret read payload")),
        put: () => Promise.reject(new Error("secret write payload")),
      },
    },
  });
  try {
    const response = await download(
      request(),
      { RELEASES: bucket() },
      { waitUntil: (pending: Promise<unknown>) => writes.push(pending) }
    );
    expect(response.status).toBe(200);
    expect(await response.text()).toBe("0123456789");
    await Promise.all(writes);
    expect(output).toHaveBeenCalledTimes(2);
    for (const line of diagnostics) {
      const record: unknown = JSON.parse(String(line));
      expect(record).toMatchObject({
        event: "edge.cache",
        error_code: "cache_unavailable",
      });
      expect(String(line)).not.toContain("secret");
      expect(String(line)).not.toContain("https:");
    }
  } finally {
    output.mockRestore();
    if (previous) {
      Object.defineProperty(globalThis, "caches", previous);
    } else {
      Reflect.deleteProperty(globalThis, "caches");
    }
  }
});
