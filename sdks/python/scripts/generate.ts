/**
 * Generate Python TypedDict models and typed sync/async resources from the shared public
 * contract. Wire field names are retained, including Python keywords. References stay forward
 * references so recursive schemas import safely. Unsupported schema shapes stop generation;
 * source models are never guessed. `--check` compares every generated file without rewriting it.
 */
import { mkdir } from "node:fs/promises";
import { parseArgs } from "node:util";

import {
  loadContract,
  operationEntries,
} from "../../../tools/codegen/contract";
import type { OperationEntry, Spec } from "../../../tools/codegen/contract";

type Schema = Record<string, unknown>;
const OUT = new URL("../src/norbelys/_generated/", import.meta.url);
const KEYWORDS = new Set([
  "from",
  "class",
  "def",
  "return",
  "import",
  "global",
  "async",
  "await",
  "in",
  "is",
  "not",
  "and",
  "or",
  "del",
  "pass",
  "raise",
  "with",
  "yield",
  "lambda",
  "None",
  "True",
  "False",
]);
const identifier = (name: string): string =>
  KEYWORDS.has(name) ? `${name}_` : name;
const className = (parts: string[]): string =>
  parts
    .map((part) =>
      part
        .split("_")
        .map((word) => word[0]?.toUpperCase() + word.slice(1))
        .join("")
    )
    .join("");
const reference = (value: unknown): string | undefined =>
  typeof value === "string" && value.startsWith("#/components/schemas/")
    ? value.slice("#/components/schemas/".length)
    : undefined;
const asSchema = (value: unknown): Schema =>
  typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Schema)
    : {};
const pyLiteral = (value: unknown): string => {
  if (value === null) {
    return "None";
  }
  if (value === true) {
    return "True";
  }
  if (value === false) {
    return "False";
  }
  return JSON.stringify(value);
};
const union = (types: string[]): string => {
  const unique = [...new Set(types)];
  return unique.length === 1
    ? (unique[0] ?? "Any")
    : `Union[${unique.join(", ")}]`;
};

/** A Python annotation for every supported JSON schema value; references remain quoted. */
const annotation = (schema: Schema): string => {
  const ref = reference(schema.$ref);
  if (ref) {
    return JSON.stringify(ref);
  }
  const choices = schema.oneOf ?? schema.anyOf;
  if (Array.isArray(choices)) {
    return union(choices.map((value) => annotation(asSchema(value))));
  }
  if (Array.isArray(schema.enum)) {
    const literal = `Literal[${schema.enum.map(pyLiteral).join(", ")}]`;
    return schema["x-open-enum"] ? union([literal, "str"]) : literal;
  }
  if (Array.isArray(schema.type)) {
    return union(schema.type.map((type) => annotation({ ...schema, type })));
  }
  switch (schema.type) {
    case "string": {
      return "str";
    }
    case "integer": {
      return "int";
    }
    case "number": {
      return "float";
    }
    case "boolean": {
      return "bool";
    }
    case "null": {
      return "None";
    }
    case "array": {
      // JSON tuple values remain lists on the wire; each position's allowed type is retained.
      return `list[${Array.isArray(schema.prefixItems) ? union(schema.prefixItems.map((value) => annotation(asSchema(value)))) : annotation(asSchema(schema.items))}]`;
    }
    case "object": {
      return `dict[str, ${annotation(asSchema(schema.additionalProperties))}]`;
    }
    case undefined: {
      return "Any";
    }
    default: {
      throw new Error(`Unsupported schema type ${String(schema.type)}.`);
    }
  }
};

/** Merge object intersections, preserving the required fields of every constituent. */
const objectShape = (
  document: Spec,
  schema: Schema,
  seen = new Set<string>()
): Schema => {
  const ref = reference(schema.$ref);
  if (ref && !seen.has(ref)) {
    const target = document.components?.schemas?.[ref];
    if (!target) {
      throw new Error(`Missing schema ${ref}.`);
    }
    return objectShape(document, target, new Set([...seen, ref]));
  }
  if (!Array.isArray(schema.allOf)) {
    return schema;
  }
  const parts = schema.allOf.map((value) =>
    objectShape(document, asSchema(value), seen)
  );
  return {
    type: "object",
    properties: Object.assign(
      {},
      ...parts.map((part) => part.properties ?? {})
    ),
    required: [
      ...new Set(
        parts.flatMap((part) =>
          Array.isArray(part.required) ? (part.required as string[]) : []
        )
      ),
    ],
  };
};

const typedDict = (
  name: string,
  properties: Record<string, Schema>,
  required: string[]
): string => {
  const fields = Object.entries(properties).map(
    ([field, value]) =>
      `    ${JSON.stringify(field)}: ${required.includes(field) ? "Required" : "NotRequired"}[${annotation(value)}],`
  );
  return `${name} = TypedDict(${JSON.stringify(name)}, {\n${fields.join("\n")}\n})\n`;
};
const responseType = (entry: OperationEntry): string => {
  const answers = Object.entries(entry.operation.responses).filter(([status]) =>
    status.startsWith("2")
  );
  if (!answers.length) {
    throw new Error(`${entry.operationId} has no success response.`);
  }
  return union(
    answers.map(([, answer]) => {
      if (!answer.content || !Object.keys(answer.content).length) {
        return "None";
      }
      if (
        Object.keys(answer.content).some(
          (media) => media !== "application/json"
        )
      ) {
        throw new Error(`${entry.operationId}: unsupported success media.`);
      }
      const ref = reference(answer.content["application/json"]?.schema?.$ref);
      if (!ref) {
        throw new Error(
          `${entry.operationId}: success requires a named schema.`
        );
      }
      return `schema.${ref}`;
    })
  );
};

/** Generate deterministic source files; both transport variants use the same operation model. */
// oxlint-disable-next-line complexity -- Schema/model/resource emission is one deterministic compiler pass.
export const generatePython = (document: Spec): Record<string, string> => {
  const entries = operationEntries(document);
  const modelLines = [
    "# Generated from the public OpenAPI contract. Do not edit.",
    '"""Static wire types. Values are ordinary JSON dictionaries; omitted and null remain distinct."""',
    "from typing import Any, Literal, NotRequired, Required, TypedDict, TypeAlias, Union",
    "",
  ];
  for (const [name, raw] of Object.entries(
    document.components?.schemas ?? {}
  )) {
    if (!/^[A-Za-z_]\w*$/u.test(name) || KEYWORDS.has(name)) {
      throw new Error(`Invalid schema name ${name}.`);
    }
    const shape = objectShape(document, raw);
    modelLines.push(
      shape.type === "object" && shape.properties
        ? typedDict(
            name,
            shape.properties as Record<string, Schema>,
            (shape.required ?? []) as string[]
          )
        : `${name}: TypeAlias = ${annotation(raw)}\n`
    );
  }
  const queryNames = new Map<string, string>();
  for (const entry of entries) {
    const query =
      entry.operation.parameters?.filter(
        (parameter) => parameter.in === "query"
      ) ?? [];
    if (!query.length) {
      continue;
    }
    const name = `${className([...entry.segments, entry.action])}Query`;
    queryNames.set(entry.operationId, name);
    modelLines.push(
      typedDict(
        name,
        Object.fromEntries(
          query.map((parameter) => [
            parameter.name,
            parameter.schema ??
              parameter.content?.["application/json"]?.schema ??
              {},
          ])
        ),
        query
          .filter(
            (parameter) => parameter.required && parameter.name !== "cursor"
          )
          .map((parameter) => parameter.name)
      )
    );
  }
  modelLines.push(
    'ImageUpload = TypedDict("ImageUpload", {"data": bytes, "content_type": Literal["image/gif", "image/jpeg", "image/png", "image/webp"]})',
    ""
  );

  const resources = new Map<
    string,
    { segments: string[]; methods: OperationEntry[]; children: Set<string> }
  >([["", { segments: [], methods: [], children: new Set() }]]);
  for (const entry of entries) {
    for (let index = 0; index < entry.segments.length; index += 1) {
      const key = entry.segments.slice(0, index + 1).join(".");
      const parent = entry.segments.slice(0, index).join(".");
      if (!resources.has(key)) {
        resources.set(key, {
          segments: entry.segments.slice(0, index + 1),
          methods: [],
          children: new Set(),
        });
      }
      resources.get(parent)?.children.add(entry.segments[index] ?? "");
    }
    resources.get(entry.segments.join("."))?.methods.push(entry);
  }
  const code = [
    "# Generated from the public OpenAPI contract. Do not edit.",
    '"""Every public API operation with the same typed interface for sync and asyncio clients."""',
    "from __future__ import annotations",
    "from collections.abc import AsyncIterator, Iterator",
    "from typing import Union, cast, overload",
    "from .._core import AsyncCore, RequestOptions, SyncCore",
    "from ..pagination import async_iter_items, iter_items",
    "from . import models as schema",
    "",
  ];
  for (const async of [false, true]) {
    const prefix = async ? "Async" : "";
    for (const resource of [...resources.values()].toReversed()) {
      const name = resource.segments.length
        ? className(resource.segments)
        : "NorbelysResources";
      code.push(
        `class ${prefix}${name}:`,
        `    """Typed operations for ${resource.segments.join(".") || "all public resources"}."""`,
        `    def __init__(self, core: ${async ? "Async" : "Sync"}Core) -> None:`,
        "        self._core = core"
      );
      for (const child of resource.children) {
        code.push(
          `        self.${identifier(child)} = ${prefix}${className([...resource.segments, child])}(core)`
        );
      }
      for (const entry of resource.methods) {
        const parameters = entry.operation.parameters ?? [];
        if (
          parameters.some(
            (parameter) =>
              parameter.in === "cookie" ||
              (parameter.in === "header" &&
                !["Idempotency-Key", "If-Match"].includes(parameter.name))
          )
        ) {
          throw new Error(`${entry.operationId}: unsupported parameter.`);
        }
        const pathArgs = [
          ...entry.path.matchAll(/\{(?<parameter>[^}]+)\}/gu),
        ].map((match) => match.groups?.parameter ?? "");
        const query = queryNames.get(entry.operationId);
        const media = entry.operation.requestBody?.content ?? {};
        const mediaTypes = Object.keys(media);
        const image =
          mediaTypes.length > 0 &&
          mediaTypes.every((type) => type.startsWith("image/"));
        if (
          !image &&
          mediaTypes.some(
            (type) => !["application/json", "text/csv"].includes(type)
          )
        ) {
          throw new Error(`${entry.operationId}: unsupported body media.`);
        }
        let bodyType: string | undefined;
        if (image) {
          bodyType = "schema.ImageUpload";
        } else if (media["application/json"]?.schema) {
          bodyType = `schema.${reference(media["application/json"].schema.$ref)}`;
        }
        if (bodyType?.endsWith("undefined")) {
          throw new Error(
            `${entry.operationId}: body requires a named schema.`
          );
        }
        const csv = mediaTypes.includes("text/csv");
        const result = responseType(entry);
        const bodyArgument = (type: string): string =>
          `body: ${csv ? `${type} | str` : type}${entry.operation.requestBody?.required ? "" : " | None = None"}`;
        const signature = (bodyOverride?: string): string =>
          [
            ...pathArgs.map((arg) => `${identifier(arg)}: str`),
            ...(bodyType ? [bodyArgument(bodyOverride ?? bodyType)] : []),
            "*",
            ...(query ? [`query: schema.${query} | None = None`] : []),
            "options: RequestOptions | None = None",
          ].join(", ");
        const overloads = entry.operation["x-norbelys-overloads"];
        if (overloads) {
          const grouped = new Map<string, string[]>();
          for (const pair of overloads) {
            const request = reference(pair.request.$ref);
            const response = reference(pair.response.$ref);
            if (!request || !response) {
              throw new Error(`${entry.operationId}: invalid overload.`);
            }
            grouped.set(response, [
              ...(grouped.get(response) ?? []),
              `schema.${request}`,
            ]);
          }
          for (const [response, requests] of grouped) {
            code.push(
              "",
              "    @overload",
              `    ${async ? "async " : ""}def ${identifier(entry.action)}(self, ${signature(requests.join(" | "))}) -> schema.${response}: ...`
            );
          }
        }
        code.push(
          "",
          `    ${async ? "async " : ""}def ${identifier(entry.action)}(self, ${signature()}) -> ${result}:`,
          `        ${JSON.stringify(entry.operation.summary ?? entry.operationId)}`
        );
        let bodyValue = bodyType ? "body" : "None";
        if (image) {
          bodyValue = 'body["data"]';
        }
        let contentType = "None";
        if (csv) {
          contentType = '"text/csv" if isinstance(body, str) else None';
        }
        if (image) {
          contentType = 'body["content_type"]';
        }
        const call = `${async ? "await " : ""}self._core.request(${JSON.stringify(entry.httpMethod.toUpperCase())}, ${JSON.stringify(entry.path)}, [${pathArgs.map(identifier).join(", ")}], query=${query ? "query" : "None"}, body=${bodyValue}, content_type=${contentType}, idempotent=${parameters.some((parameter) => parameter.name === "Idempotency-Key") ? "True" : "False"}, options=options)`;
        code.push(`        return cast(${JSON.stringify(result)}, ${call})`);
        const paginated = parameters.some(
          (parameter) => parameter.in === "query" && parameter.name === "cursor"
        );
        if (paginated && entry.action === "list") {
          const ref = Object.values(entry.operation.responses).find(
            (answer) => answer.content?.["application/json"]
          )?.content?.["application/json"]?.schema?.$ref;
          const collection = objectShape(
            document,
            document.components?.schemas?.[reference(ref) ?? ""] ?? {}
          );
          const { items } = asSchema(asSchema(collection.properties).data);
          const item = `${className([...entry.segments, entry.action])}Item`;
          if (!items || !query) {
            throw new Error(
              `${entry.operationId}: collection requires typed data items and query.`
            );
          }
          if (!async) {
            modelLines.push(
              `${item}: TypeAlias = ${annotation(asSchema(items))}\n`
            );
          }
          code.push(
            "",
            `    def iter(self, *, query: schema.${query} | None = None, options: RequestOptions | None = None) -> ${async ? "AsyncIterator" : "Iterator"}[schema.${item}]:`,
            '        """Fetch pages lazily and yield every item while preserving the original filters."""',
            `        return ${async ? "async_iter_items" : "iter_items"}(lambda cursor: self.list(query=cast(schema.${query}, {**(query or {}), "cursor": cursor}), options=options))`
          );
        }
      }
      code.push("");
    }
  }
  return {
    "__init__.py": '"""Generated public API models and operations."""\n',
    "models.py": modelLines.join("\n"),
    "resources.py": code.join("\n"),
  };
};

if (import.meta.main) {
  const { values } = parseArgs({
    options: {
      spec: { type: "string" },
      check: { type: "boolean", default: false },
    },
  });
  const document = await loadContract(values.spec);
  const files = generatePython(document);
  if (!values.check) {
    await mkdir(OUT, { recursive: true });
  }
  await Promise.all(
    Object.entries(files).map(async ([name, content]) => {
      const output = new URL(name, OUT);
      if (values.check) {
        if (
          !(await Bun.file(output).exists()) ||
          (await Bun.file(output).text()) !== content
        ) {
          throw new Error(
            `${name} differs from the public contract; regenerate the Python SDK.`
          );
        }
      } else {
        await Bun.write(output, content);
      }
    })
  );
  process.stdout.write(
    `${values.check ? "Verified" : "Generated"} ${operationEntries(document).length} Python operations.\n`
  );
}
