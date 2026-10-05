import { expect, test } from "bun:test";

import braces from "braces";
import CachePolicy from "http-cache-semantics";

// Direct dev dependencies exercise the same version-pinned patches used by transitive tooling.
test("braces bounds nested braces, parentheses and direct AST traversals", () => {
  for (const [open, close] of [
    ["{", "}"],
    ["(", ")"],
  ]) {
    const pattern = `${open.repeat(4000)}a${close.repeat(4000)}`;
    for (const operation of [
      braces,
      braces.parse,
      braces.compile,
      braces.expand,
      braces.stringify,
    ]) {
      expect(() => operation(pattern)).toThrow(
        "Pattern nesting exceeds maximum depth (100)"
      );
    }
  }
  let ast = { type: "text", value: "a" };
  for (let depth = 0; depth < 4000; depth++) {
    ast = { type: "root", nodes: [ast] };
  }
  for (const operation of [braces.compile, braces.expand, braces.stringify]) {
    expect(() => operation(ast)).toThrow(
      "Pattern nesting exceeds maximum depth (100)"
    );
  }
  expect(braces.expand("src/{app,web}/{a,b}.ts")).toEqual([
    "src/app/a.ts",
    "src/app/b.ts",
    "src/web/a.ts",
    "src/web/b.ts",
  ]);
  expect(braces.compile("{a,b}")).toBe("(a|b)");
  expect(braces.stringify("{a,b}")).toBe("{a,b}");
  expect(braces.compile("\\{".repeat(200))).toBe("{".repeat(200));
});

const request = {
  url: "https://cache.example.test/item",
  method: "GET",
  headers: { host: "cache.example.test" },
};
const staleRequest = {
  ...request,
  headers: { ...request.headers, "cache-control": "max-stale=999999" },
};
const policyFor = (headers, options) => {
  const policy = new CachePolicy(request, { status: 200, headers }, options);
  policy.now = () => policy._responseTime + 10_000;
  return policy;
};

test("max-stale never bypasses zero-lifetime cache restrictions", () => {
  for (const headers of [
    { "set-cookie": "session=fixture", "cache-control": "max-age=60" },
    { "cache-control": "private, max-age=60" },
    { "cache-control": "no-store" },
    { "cache-control": "no-cache, max-age=60" },
    { "cache-control": "proxy-revalidate, max-age=60" },
    { "cache-control": "max-age=0" },
  ]) {
    const policy = policyFor(headers);
    expect(policy.satisfiesWithoutRevalidation(staleRequest)).toBe(false);
    expect(policy.evaluateRequest(staleRequest).response).toBeUndefined();
    expect(policy.evaluateRequest(staleRequest).revalidation.synchronous).toBe(
      true
    );
    const unlimited = {
      ...staleRequest,
      headers: { ...staleRequest.headers, "cache-control": "max-stale" },
    };
    expect(policy.satisfiesWithoutRevalidation(unlimited)).toBe(false);
  }
});

test("stale error and background revalidation paths respect the same restrictions", () => {
  for (const prefix of [
    "private",
    "no-cache",
    "no-store",
    "proxy-revalidate",
    "max-age=0",
    "must-revalidate",
  ]) {
    const policy = policyFor({
      "cache-control": `${prefix}, stale-if-error=600, stale-while-revalidate=600`,
    });
    expect(policy.useStaleWhileRevalidate()).toBe(false);
    expect(policy.evaluateRequest(request).response).toBeUndefined();
    expect(
      policy.revalidatedPolicy(request, { status: 500, headers: {} }).modified
    ).toBe(true);
  }
  const cookie = policyFor({
    "set-cookie": "session=fixture",
    "cache-control":
      "max-age=60, stale-if-error=600, stale-while-revalidate=600",
  });
  expect(cookie.evaluateRequest(staleRequest).response).toBeUndefined();
  expect(
    cookie.revalidatedPolicy(request, { status: 500, headers: {} }).modified
  ).toBe(true);
});

test("ordinary expired public responses retain stale caching", () => {
  const policy = policyFor({
    "cache-control":
      "public, max-age=1, stale-if-error=600, stale-while-revalidate=600",
  });
  expect(policy.satisfiesWithoutRevalidation(staleRequest)).toBe(true);
  expect(policy.useStaleWhileRevalidate()).toBe(true);
  expect(
    policy.revalidatedPolicy(request, { status: 500, headers: {} }).modified
  ).toBe(false);
  const privateCache = policyFor(
    { "cache-control": "private, max-age=60", "set-cookie": "session=fixture" },
    { shared: false }
  );
  expect(privateCache.satisfiesWithoutRevalidation(request)).toBe(true);
});
