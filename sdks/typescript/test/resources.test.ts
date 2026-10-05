import { describe, expect, test } from "bun:test";

import { Norbelys } from "../src/index";
import { fakeFetch, header, jsonBody, lastPage, respond } from "./support";

interface Operation {
  operationId: string;
  parameters?: { in: string; name: string }[];
  requestBody?: { content: Record<string, { example?: unknown }> };
}
const spec = (await Bun.file(
  new URL("../../../crates/server/openapi.json", import.meta.url)
).json()) as {
  paths: Record<string, Record<string, Operation>>;
};
const operations = Object.entries(spec.paths).flatMap(([path, item]) =>
  Object.entries(item).map(([method, operation]) => ({
    path,
    method,
    operation,
  }))
);
const camel = (name: string): string =>
  name.replaceAll(/_(?<letter>[a-z])/gu, (_match, letter: string) =>
    letter.toUpperCase()
  );

/** The dotted names of every method reachable from the client. */
const methodNames = (target: object, prefix = ""): string[] => {
  const own = Object.getOwnPropertyNames(Object.getPrototypeOf(target)).filter(
    (name) =>
      name !== "constructor" && typeof Reflect.get(target, name) === "function"
  );
  return [
    ...own.map((name) => `${prefix}${name}`),
    ...Object.entries(target as Record<string, unknown>).flatMap(
      ([name, child]) =>
        typeof child === "object" && child !== null
          ? methodNames(child, `${prefix}${name}.`)
          : []
    ),
  ];
};

/** Resolves a public operation to the generated method, retaining its resource receiver. */
const invoke = (client: Norbelys, id: string, args: unknown[]): unknown => {
  const parts = id.split(".");
  const method = parts.pop();
  let resource: object = client;
  for (const part of parts) {
    resource = Reflect.get(resource, camel(part)) as object;
  }
  const call: unknown =
    method === undefined ? undefined : Reflect.get(resource, camel(method));
  if (typeof call !== "function") {
    throw new TypeError(`No generated method for ${id}`);
  }
  return Reflect.apply(call, resource, args);
};

describe("resources", () => {
  test("exposes exactly the public operations, with no dashboard credentials", () => {
    expect(
      methodNames(new Norbelys({ apiKey: "nb_test_example" })).toSorted()
    ).toEqual(
      operations
        .map(({ operation }) => {
          const parts = operation.operationId.split(".");
          const method = parts.pop();
          return [...parts, method].map((part) => camel(part ?? "")).join(".");
        })
        .toSorted()
    );
  });

  test.each(operations)(
    "$operation.operationId sends the published request",
    async ({ path, method, operation }) => {
      const { fetch, calls } = fakeFetch(lastPage());
      const args: unknown[] = [...path.matchAll(/\{[^}]+\}/gu)].map(
        () => "id/with spaces"
      );
      const content = operation.requestBody?.content;
      const imageType = Object.keys(content ?? {}).find((type) =>
        type.startsWith("image/")
      );
      const body = content?.["application/json"]?.example ?? {};
      const blob = new Blob(["image bytes"]);
      if (content) {
        args.push(imageType ? { data: blob, contentType: imageType } : body);
      }
      const query = operation.parameters?.some(
        (parameter) => parameter.in === "query"
      );
      if (query) {
        args.push({ limit: 2 });
      }
      await invoke(
        new Norbelys({ apiKey: "nb_test_example", fetch }),
        operation.operationId,
        args
      );

      expect(calls).toHaveLength(1);
      const [call] = calls;
      const url = new URL(call?.url ?? "");
      expect(call?.init.method).toBe(method.toUpperCase());
      expect(url.pathname).toBe(
        path.replaceAll(/\{[^}]+\}/gu, "id%2Fwith%20spaces")
      );
      expect(Object.fromEntries(url.searchParams)).toEqual(
        query ? { limit: "2" } : {}
      );
      if (imageType) {
        expect(call?.init.body).toBe(blob);
        expect(header(call, "content-type")).toBe(imageType);
      } else {
        expect(jsonBody(call)).toEqual(content ? body : undefined);
      }
      expect(header(call, "idempotency-key") !== null).toBe(
        operation.parameters?.some(
          (parameter) => parameter.name === "Idempotency-Key"
        ) ?? false
      );
    }
  );

  test("imports send raw CSV and a group query without JSON encoding the file", async () => {
    const { fetch, calls } = fakeFetch(respond(202, { id: "imp_1" }));
    const csv = "email\nada@example.com\n";
    await new Norbelys({ apiKey: "nb_test_example", fetch }).imports.create(
      csv,
      { group_id: "grp_1" }
    );
    expect(calls[0]?.init.body).toBe(csv);
    expect(header(calls[0], "content-type")).toBe("text/csv");
    expect(new URL(calls[0]?.url ?? "").searchParams.get("group_id")).toBe(
      "grp_1"
    );
    expect(header(calls[0], "idempotency-key")).toBeString();
  });

  test("updates preserve If-Match and create a retry key for one logical change", async () => {
    const { fetch, calls } = fakeFetch(respond(200, {}));
    await new Norbelys({ apiKey: "nb_test_example", fetch }).groups.update(
      "grp_1",
      { name: "Updated" },
      { headers: { "If-Match": '"123"' } }
    );
    expect(header(calls[0], "if-match")).toBe('"123"');
    expect(header(calls[0], "idempotency-key")).toBeString();
  });

  test("resource namespaces are stable objects", () => {
    const client = new Norbelys({ apiKey: "nb_test_example" });
    expect(client.enrollments).toBe(client.enrollments);
    expect(client.imports).toBe(client.imports);
  });
});
