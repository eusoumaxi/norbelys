import { APIConnectionError, APIError } from "@norbelys/sdk";

interface ProblemText {
  title: string;
  detail: string;
  /** Quote it when reporting the problem. */
  requestId?: string;
}

const UNAVAILABLE = new Set([502, 503, 504]);

/** What to tell a person about a failed request, in plain words. */
export const describeProblem = (error: unknown): ProblemText => {
  if (error instanceof APIConnectionError) {
    return {
      detail: error.timeout
        ? "The request took too long. Try again."
        : "Check your connection and try again.",
      title: "Can't reach Norbelys",
    };
  }
  if (error instanceof APIError) {
    if (!error.title && UNAVAILABLE.has(error.status)) {
      return {
        detail: "The API is not answering right now. Try again in a moment.",
        requestId: error.requestId,
        title: "Norbelys is unavailable",
      };
    }
    return {
      detail: error.detail ?? `The API answered ${error.status}.`,
      requestId: error.requestId,
      title: error.title ?? "Something went wrong",
    };
  }
  return {
    detail: "An unexpected error occurred.",
    title: "Something went wrong",
  };
};

/** A failure in one line, with the reference to quote: `Not found. (Reference req_…)`. */
export const problemLine = (error: unknown): string => {
  const { detail, requestId } = describeProblem(error);
  return requestId ? `${detail} (Reference ${requestId})` : detail;
};

/**
 * The API's problems with the fields of a refused request, by path (`smtp.host`,
 * `steps[0].variants[1].subject`); none for any other failure.
 */
export const fieldProblems = (
  error: unknown
): Readonly<Record<string, string>> =>
  error instanceof APIError ? error.fields : {};

/** Whether a problem's path is the field at `field` or one of its parts (`identities[2].email`). */
const under = (path: string, field: string): boolean =>
  path === field ||
  path.startsWith(`${field}.`) ||
  path.startsWith(`${field}[`);

/** The first problem of the field at `field` or of one of its parts. */
export const problemAt = (
  problems: Readonly<Record<string, string>>,
  field: string
): string | undefined =>
  Object.entries(problems).find(([path]) => under(path, field))?.[1];

/** The problems no field among `fields` shows, as `[path, problem]`. */
export const unplacedProblems = (
  problems: Readonly<Record<string, string>>,
  fields: readonly string[]
): [string, string][] =>
  Object.entries(problems).filter(
    ([path]) => !fields.some((field) => under(path, field))
  );
