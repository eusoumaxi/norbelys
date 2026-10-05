/**
 * The public contract shared by SDK generation. The committed document is the default input:
 * generating a client never starts the server or needs a database. Server checks independently
 * prove that this document matches the handlers. Explicit file and HTTP inputs support external
 * consumers. Operations are validated here so each language receives the same route identities.
 */
import { fileURLToPath } from "node:url";

const METHODS = ["get", "post", "put", "patch", "delete"] as const;
export type HttpMethod = (typeof METHODS)[number];
export interface Reference {
  $ref?: string;
}
export interface Media {
  schema?: Reference & Record<string, unknown>;
}
export interface Parameter {
  name: string;
  in: "path" | "query" | "header" | "cookie";
  required?: boolean;
  description?: string;
  schema?: Record<string, unknown>;
  content?: Record<string, Media>;
}
export interface Answer {
  description?: string;
  content?: Record<string, Media>;
}
export interface Overload {
  request: Reference;
  response: Reference;
}
export interface Operation {
  operationId?: string;
  summary?: string;
  description?: string;
  parameters?: Parameter[];
  requestBody?: { required?: boolean; content?: Record<string, Media> };
  responses: Record<string, Answer>;
  "x-norbelys-overloads"?: Overload[];
}
export interface Spec {
  paths: Record<string, Partial<Record<HttpMethod, Operation>>>;
  components?: { schemas?: Record<string, Record<string, unknown>> };
}
export interface OperationEntry {
  path: string;
  httpMethod: HttpMethod;
  operationId: string;
  segments: string[];
  action: string;
  operation: Operation;
}

/** Read an explicit contract or the repository's canonical public JSON document. */
export const loadContract = async (source?: string): Promise<Spec> => {
  if (source && /^https?:\/\//u.test(source)) {
    const response = await fetch(source);
    if (!response.ok) {
      throw new Error(`GET ${source} returned HTTP ${response.status}.`);
    }
    return (await response.json()) as Spec;
  }
  return (await Bun.file(
    source ??
      fileURLToPath(
        new URL("../../crates/server/openapi.json", import.meta.url)
      )
  ).json()) as Spec;
};

/** Validate unique dotted operation ids and return every client operation exactly once. */
export const operationEntries = (document: Spec): OperationEntry[] => {
  const ids = new Set<string>();
  const entries: OperationEntry[] = [];
  for (const [path, item] of Object.entries(document.paths)) {
    for (const httpMethod of METHODS) {
      const operation = item[httpMethod];
      if (!operation || operation.operationId?.startsWith("health.")) {
        continue;
      }
      const { operationId } = operation;
      if (!operationId) {
        throw new Error(
          `${httpMethod.toUpperCase()} ${path} has no operationId.`
        );
      }
      if (!/^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)+$/u.test(operationId)) {
        throw new Error(
          `${httpMethod.toUpperCase()} ${path}: operationId must look like resource.method.`
        );
      }
      if (ids.has(operationId)) {
        throw new Error(`Duplicate operationId ${operationId}.`);
      }
      ids.add(operationId);
      const segments = operationId.split(".");
      const action = segments.pop();
      if (!action) {
        throw new Error(`Invalid operationId ${operationId}.`);
      }
      entries.push({
        path,
        httpMethod,
        operationId,
        segments,
        action,
        operation,
      });
    }
  }
  return entries;
};
