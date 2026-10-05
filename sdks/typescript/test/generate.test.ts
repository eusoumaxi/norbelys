import { describe, expect, test } from "bun:test";

import type { Spec } from "../scripts/generate";
import { generateResources } from "../scripts/generate";

type Paths = Spec["paths"];
type Operation = NonNullable<Paths[string]["get"]>;

const ok = (schema: string): Operation["responses"] => ({
  "200": {
    content: {
      "application/json": {
        schema: { $ref: `#/components/schemas/${schema}` },
      },
    },
  },
});

const body = (
  schema: string,
  required = true
): NonNullable<Operation["requestBody"]> => ({
  content: {
    "application/json": { schema: { $ref: `#/components/schemas/${schema}` } },
  },
  required,
});

const code = (paths: Paths, schemas: Record<string, object> = {}): string =>
  generateResources({
    components: { schemas: schemas as Record<string, Record<string, unknown>> },
    paths,
  }).code;

/** One `x-norbelys-overloads` entry: a request form and its answer. */
const form = (request: string, response: string): object => ({
  request: { $ref: `#/components/schemas/${request}` },
  response: { $ref: `#/components/schemas/${response}` },
});

const fails = (paths: Paths, message: string): void => {
  expect(() => generateResources({ paths })).toThrow(message);
};

/** The emitted lines, trimmed, for exact assertions on signatures. */
const lines = (source: string): string[] =>
  source.split("\n").map((line) => line.trim());

describe("generateResources", () => {
  test("names methods after dotted operationIds and nests their resources", () => {
    const source = code({
      "/v1/campaigns/{id}/enrollments": {
        get: {
          operationId: "campaigns.enrollments.list",
          responses: ok("Enrollments"),
        },
      },
      "/v1/tracking_domains/{id}": {
        get: { operationId: "tracking_domains.get", responses: ok("Domain") },
      },
    });

    expect(source).toContain("export class CampaignsEnrollments {");
    expect(source).toContain("readonly enrollments: CampaignsEnrollments;");
    expect(source).toContain("readonly trackingDomains: TrackingDomains;");
    expect(source).toContain(
      "readonly campaigns: Campaigns;\n  readonly trackingDomains: TrackingDomains;"
    );
    expect(source).toContain("export class NorbelysResources {");
  });

  test("names actions in camelCase, as resources are", () => {
    const source = code({
      "/v1/webhook_endpoints/{id}/rotate_secret": {
        post: {
          operationId: "webhook_endpoints.rotate_secret",
          responses: ok("Endpoint"),
        },
      },
    });

    expect(lines(source)).toContain(
      "rotateSecret(id: string, options?: RequestOptions): Promise<schema.Endpoint> {"
    );
    expect(source).not.toContain("rotate_secret(");
  });

  test("answers every success: a union, and undefined for one without a body", () => {
    const source = code({
      "/v1/campaigns/{id}": {
        delete: {
          operationId: "campaigns.delete",
          responses: {
            "200": {
              ...ok("Campaign")["200"],
              description: "It had sent: archived.",
            },
            "204": { description: "It never sent: removed." },
          },
        },
      },
      "/v1/connections": {
        post: {
          operationId: "connections.create",
          requestBody: body("NewConnection"),
          responses: {
            "201": ok("Connection")["200"] ?? {},
            "202": ok("Consent")["200"] ?? {},
          },
        },
      },
    });

    expect(lines(source)).toContain(
      "delete(id: string, options?: RequestOptions): Promise<schema.Campaign | undefined> {"
    );
    expect(lines(source)).toContain(
      "create(body: schema.NewConnection, options?: RequestOptions): Promise<schema.Connection | schema.Consent> {"
    );
    expect(source).toContain(
      [
        "   * Answers one of:",
        "   * - `200` (`Campaign`): It had sent: archived.",
        "   * - `204` (no body, `undefined`): It never sent: removed.",
      ].join("\n")
    );
  });

  test("gives an update guarded by If-Match the ifMatch option, and imports only what it uses", () => {
    const update = code({
      "/v1/groups/{id}": {
        patch: {
          operationId: "groups.update",
          parameters: [
            { in: "path", name: "id", required: true },
            { in: "header", name: "If-Match" },
          ],
          requestBody: body("GroupPatch"),
          responses: ok("Group"),
        },
      },
    });
    const read = code({
      "/v1/groups/{id}": {
        get: { operationId: "groups.retrieve", responses: ok("Group") },
      },
    });

    expect(lines(update)).toContain(
      "update(id: string, body: schema.GroupPatch, options?: UpdateOptions): Promise<schema.Group> {"
    );
    expect(update).toContain(
      'import type { Core, RequestOptions, UpdateOptions } from "../core";'
    );
    expect(read).toContain(
      'import type { Core, RequestOptions } from "../core";'
    );
  });

  test("documents each path parameter with its description", () => {
    const source = code({
      "/v1/campaigns/{id}/enrollments/{enrollment_id}": {
        get: {
          operationId: "campaigns.enrollments.get",
          parameters: [
            {
              in: "path",
              name: "id",
              description: "The campaign id (`cmp_…`).",
            },
            {
              in: "path",
              name: "enrollment_id",
              description: "The enrollment\nid.",
            },
          ],
          responses: ok("Enrollment"),
        },
      },
    });

    expect(source).toContain(
      [
        "   * @param id - The campaign id (`cmp_…`).",
        "   * @param enrollmentId - The enrollment id.",
        "   */",
      ].join("\n")
    );
  });

  test("counts methods and skips health checks", () => {
    const result = generateResources({
      paths: {
        "/health/ready": {
          get: { operationId: "health.ready.get", responses: ok("Health") },
        },
        "/v1/groups": {
          get: { operationId: "groups.list", responses: ok("Groups") },
          post: {
            operationId: "groups.create",
            requestBody: body("NewGroup"),
            responses: ok("Group"),
          },
        },
      },
    });

    expect(result.methods).toBe(2);
    expect(result.code).not.toContain("health");
  });

  test("passes path ids positionally, in camelCase", () => {
    const source = code({
      "/v1/campaigns/{id}/enrollments/{enrollment_id}": {
        get: {
          operationId: "campaigns.enrollments.get",
          responses: ok("Enrollment"),
        },
      },
    });

    expect(lines(source)).toContain(
      "get(id: string, enrollmentId: string, options?: RequestOptions): Promise<schema.Enrollment> {"
    );
    expect(source).toContain("[id, enrollmentId], {}, options)");
  });

  test("returns a PagePromise for lists, starting at query.cursor", () => {
    const source = code({
      "/v1/groups": {
        get: {
          operationId: "groups.list",
          parameters: [
            { in: "query", name: "page_size" },
            { in: "query", name: "cursor" },
          ],
          responses: ok("Groups"),
        },
      },
    });

    expect(source).toContain(
      'list(query?: NonNullable<operations["groups.list"]["parameters"]["query"]>, options?: RequestOptions): PagePromise<schema.Groups> {'
    );
    expect(source).toContain(
      "new PagePromise((cursor, pageOptions) => this.#core.request<schema.Groups>("
    );
    expect(source).toContain(
      "{ query: { ...query, cursor: cursor ?? query?.cursor } }, { ...options, ...pageOptions })"
    );
  });

  test("makes the query required when one of its parameters is", () => {
    const source = code({
      "/v1/reports": {
        get: {
          operationId: "reports.get",
          parameters: [{ in: "query", name: "from", required: true }],
          responses: ok("Report"),
        },
      },
    });

    expect(source).toContain(
      'get(query: NonNullable<operations["reports.get"]["parameters"]["query"]>, options?: RequestOptions)'
    );
  });

  test("sends an Idempotency-Key only where the operation declares one", () => {
    const source = code({
      "/v1/messages": {
        post: {
          operationId: "messages.create",
          parameters: [{ in: "header", name: "Idempotency-Key" }],
          requestBody: body("NewMessage"),
          responses: ok("Message"),
        },
      },
      "/v1/groups": {
        post: {
          operationId: "groups.create",
          requestBody: body("NewGroup"),
          responses: ok("Group"),
        },
      },
    });

    expect(source).toContain(
      '{ method: "POST", path: "/v1/messages", idempotent: true }'
    );
    expect(source).toContain(
      '{ method: "POST", path: "/v1/groups", idempotent: false }'
    );
  });

  test("uses schema names for bodies and answers, and marks optional bodies", () => {
    const source = code({
      "/v1/groups/{id}": {
        patch: {
          operationId: "groups.update",
          requestBody: body("GroupPatch", false),
          responses: ok("Group"),
        },
      },
    });

    expect(lines(source)).toContain(
      "update(id: string, body?: schema.GroupPatch, options?: RequestOptions): Promise<schema.Group> {"
    );
  });

  test("accepts exactly one form of a union body", () => {
    const source = code(
      {
        "/v1/messages": {
          post: {
            operationId: "messages.create",
            requestBody: body("CreateMessages"),
            responses: ok("Message"),
          },
        },
      },
      {
        CreateMessages: {
          oneOf: [
            { $ref: "#/components/schemas/Direct" },
            { $ref: "#/components/schemas/Batch" },
          ],
        },
      }
    );

    expect(source).toContain("create(body: OneOf<schema.CreateMessages>,");
  });

  test("types JSON query parameters with their schema", () => {
    const source = code({
      "/v1/people": {
        get: {
          operationId: "people.list",
          parameters: [
            { in: "query", name: "q" },
            { in: "query", name: "cursor" },
            {
              content: {
                "application/json": {
                  schema: { $ref: "#/components/schemas/PeopleFilter" },
                },
              },
              description: "A */ filter",
              in: "query",
              name: "filter",
            },
          ],
          responses: ok("People"),
        },
      },
    });

    expect(source).toContain(
      'query?: Omit<NonNullable<operations["people.list"]["parameters"]["query"]>, "filter"> & { /** A *\\/ filter */ filter?: schema.PeopleFilter }'
    );
  });

  test("emits one overload per answer from x-norbelys-overloads, then a catch-all", () => {
    const source = code({
      "/v1/messages": {
        post: {
          operationId: "messages.create",
          requestBody: body("CreateMessages"),
          responses: ok("CreatedMessages"),
          "x-norbelys-overloads": [
            form("DirectMessage", "OutboundMessage"),
            form("NewMessage", "OutboundMessage"),
            form("NewBatch", "BatchResult"),
          ],
        } as Operation,
      },
    });

    const signatures = lines(source).filter((line) =>
      line.startsWith("create(")
    );
    expect(signatures).toEqual([
      "create(body: OneOf<schema.CreateMessages, schema.DirectMessage | schema.NewMessage>, options?: RequestOptions): Promise<schema.OutboundMessage>;",
      "create(body: OneOf<schema.CreateMessages, schema.NewBatch>, options?: RequestOptions): Promise<schema.BatchResult>;",
      "create(body: OneOf<schema.CreateMessages>, options?: RequestOptions): Promise<schema.CreatedMessages>;",
      "create(body: schema.CreateMessages, options?: RequestOptions): Promise<schema.CreatedMessages> {",
    ]);
  });

  test("answers 204 with void", () => {
    const source = code({
      "/v1/groups/{id}": {
        delete: { operationId: "groups.delete", responses: { "204": {} } },
      },
    });

    expect(lines(source)).toContain(
      "delete(id: string, options?: RequestOptions): Promise<void> {"
    );
  });

  test("documents each method with its summary, description and route", () => {
    const source = code({
      "/v1/groups": {
        post: {
          description: "Names need not be unique.\nEnds a comment: */",
          operationId: "groups.create",
          requestBody: body("NewGroup"),
          responses: ok("Group"),
          summary: "Create a group",
        },
      },
    });

    expect(source).toContain(
      [
        "  /**",
        "   * Create a group",
        "   *",
        "   * Names need not be unique.",
        "   * Ends a comment: *\\/",
        "   *",
        "   * `POST /v1/groups`",
        "   */",
      ].join("\n")
    );
  });

  test("imports its helpers without file extensions", () => {
    const source = code({});

    expect(source).toContain('import { PagePromise } from "../pagination";');
    expect(source).not.toMatch(/from "[^"]+\.ts"/u);
  });
});

describe("generateResources refuses shapes it cannot type", () => {
  test("an operation without operationId", () => {
    fails(
      { "/v1/groups": { get: { responses: ok("Groups") } } },
      "has no operationId"
    );
  });

  test("an operationId without a resource", () => {
    fails(
      {
        "/v1/groups": { get: { operationId: "list", responses: ok("Groups") } },
      },
      "must look like resource.method"
    );
  });

  test("cookie and unknown header parameters", () => {
    fails(
      {
        "/v1/groups": {
          get: {
            operationId: "groups.list",
            parameters: [{ in: "cookie", name: "session" }],
            responses: ok("Groups"),
          },
        },
      },
      "unsupported parameters session"
    );
    fails(
      {
        "/v1/groups": {
          get: {
            operationId: "groups.list",
            parameters: [{ in: "header", name: "X-Tenant" }],
            responses: ok("Groups"),
          },
        },
      },
      "unsupported parameters X-Tenant"
    );
  });

  test("a body together with query parameters", () => {
    const source = code({
      "/v1/groups": {
        post: {
          operationId: "groups.create",
          parameters: [{ in: "query", name: "dry_run" }],
          requestBody: body("NewGroup"),
          responses: ok("Group"),
        },
      },
    });
    expect(source).toContain(
      'body: schema.NewGroup, query?: NonNullable<operations["groups.create"]["parameters"]["query"]>'
    );
    expect(source).toContain("{ body, query }");
  });

  test("a success whose body is not JSON", () => {
    fails(
      {
        "/v1/exports/{id}": {
          get: {
            operationId: "exports.download",
            responses: { "200": { content: { "text/csv": { schema: {} } } } },
          },
        },
      },
      "unsupported 200 response media text/csv"
    );
  });

  test("a list that answers more than one success", () => {
    fails(
      {
        "/v1/groups": {
          get: {
            operationId: "groups.list",
            parameters: [{ in: "query", name: "cursor" }],
            responses: {
              "200": ok("Groups")["200"] ?? {},
              "202": ok("Job")["200"] ?? {},
            },
          },
        },
      },
      "a list answers one success"
    );
  });

  test("an operation without a success answer", () => {
    fails(
      {
        "/v1/groups": {
          get: { operationId: "groups.list", responses: { "404": {} } },
        },
      },
      "no 2xx response"
    );
  });

  test("overloads without a body, or with inline schemas", () => {
    fails(
      {
        "/v1/groups": {
          get: {
            operationId: "groups.get",
            responses: ok("Group"),
            "x-norbelys-overloads": [],
          } as Operation,
        },
      },
      "overloads need a request body"
    );
    fails(
      {
        "/v1/messages": {
          post: {
            operationId: "messages.create",
            requestBody: body("CreateMessages"),
            responses: ok("CreatedMessages"),
            "x-norbelys-overloads": [
              {
                request: { $ref: "#/paths/x" },
                response: { $ref: "#/paths/y" },
              },
            ],
          } as Operation,
        },
      },
      "overloads must reference component schemas"
    );
  });

  test("a JSON query parameter without a schema reference", () => {
    fails(
      {
        "/v1/people": {
          get: {
            operationId: "people.list",
            parameters: [
              {
                content: { "application/json": { schema: {} } },
                in: "query",
                name: "filter",
              },
            ],
            responses: ok("People"),
          },
        },
      },
      "query parameter filter must be JSON with a schema reference"
    );
  });
});
