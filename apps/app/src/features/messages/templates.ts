/**
 * Message templates as the dashboard reads them: cutting a template into its tags and the text
 * between them, reading one tag (a printed value with its filters, an `if` and its condition),
 * and rendering a template in the browser for a preview.
 *
 * The server renders templates with MiniJinja; this module reads the part of that language a
 * message usually holds, so the editor can show a tag in words and the preview can fill it in:
 * printed paths (`{{ person.company | default("your team") }}`), the filters `default`, `upper`,
 * `lower`, `trim`, `title`, `capitalize`, `split`, `first` and `last`, and `{% if %}` blocks with `elif`, `else`, `not`,
 * `and`, `or`, `==` and `!=`. What it does not know is shown as written. A value the person
 * lacks, printed without a fallback, is marked: the server refuses to create such a message.
 * The exact rendering is the server's; a test send shows it.
 */

/** A template tag, rendered by the server and left as written: `{{ person.given_name }}`, `{% if … %}`. */
const TAG = /\{\{.*?\}\}|\{%.*?%\}/gu;

/** One stretch of a template: a tag, or the text between tags. `from` is where it starts. */
interface TemplatePart {
  from: number;
  tag: boolean;
  text: string;
}

/** `text` cut into its template tags (each on one line) and the text between them, in order. */
export const templateParts = (text: string): TemplatePart[] => {
  const parts: TemplatePart[] = [];
  let from = 0;
  for (const match of text.matchAll(TAG)) {
    if (match.index > from) {
      parts.push({ from, tag: false, text: text.slice(from, match.index) });
    }
    parts.push({ from: match.index, tag: true, text: match[0] });
    from = match.index + match[0].length;
  }
  if (from < text.length) {
    parts.push({ from, tag: false, text: text.slice(from) });
  }
  return parts;
};

/** A path a template prints: `person.given_name`, `variables.opener`. */
export const PATH = /^[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*$/u;

/** `text` split at `separator` where it is not inside quotes, each part trimmed. */
const splitOutside = (text: string, separator: string): string[] => {
  const parts: string[] = [];
  let quote: string | null = null;
  let start = 0;
  for (let index = 0; index < text.length; index += 1) {
    const character = text.charAt(index);
    if (quote) {
      quote = character === quote ? null : quote;
    } else if (character === '"' || character === "'") {
      quote = character;
    } else if (text.startsWith(separator, index)) {
      parts.push(text.slice(start, index));
      start = index + separator.length;
      index = start - 1;
    }
  }
  parts.push(text.slice(start));
  return parts.map((part) => part.trim());
};

/** A literal of a template (a quoted string, a number, a boolean or none), or `undefined`. */
export const literal = (text: string): { value: unknown } | undefined => {
  if (/^"(?:[^"\\]|\\.)*"$/u.test(text)) {
    try {
      return { value: JSON.parse(text) };
    } catch {
      return { value: text.slice(1, -1) };
    }
  }
  if (/^'[^']*'$/u.test(text)) {
    return { value: text.slice(1, -1) };
  }
  if (/^-?\d+(?:\.\d+)?$/u.test(text)) {
    return { value: Number(text) };
  }
  if (text === "true" || text === "false") {
    return { value: text === "true" };
  }
  return text === "none" ? { value: null } : undefined;
};

/** One filter of a printed expression: `default("there")` is `default` with `"there"`. */
interface Filter {
  name: string;
  argument?: string;
}

/** A filter as written, read into its name and argument. */
const readFilter = (text: string): Filter => {
  const { argument, name = text } =
    /^(?<name>\w+)\s*(?:\((?<argument>[\s\S]*)\))?$/u.exec(text)?.groups ?? {};
  return argument === undefined ? { name } : { argument, name };
};

/** A printed expression read: what it prints, then its filters in order. */
export const readExpression = (
  expression: string
): { operand: string; filters: Filter[] } => {
  const [operand = "", ...filters] = splitOutside(expression, "|");
  return { filters: filters.map(readFilter), operand };
};

/** A template tag read: a printed expression, a statement with its keyword, or a comment. */
type ReadTag =
  | { type: "print"; expression: string }
  | { type: "statement"; keyword: string; rest: string }
  | { type: "other" };

const PRINTED = /^\{\{-?(?<inner>[\s\S]*?)-?\}\}$/u;
const STATEMENT = /^\{%-?\s*(?<keyword>\w+)\s*(?<rest>[\s\S]*?)\s*-?%\}$/u;

/** One tag, read. */
export const readTag = (source: string): ReadTag => {
  const printed = PRINTED.exec(source)?.groups?.inner;
  if (printed !== undefined) {
    return { expression: printed.trim(), type: "print" };
  }
  const statement = STATEMENT.exec(source)?.groups;
  return statement?.keyword
    ? {
        keyword: statement.keyword,
        rest: statement.rest ?? "",
        type: "statement",
      }
    : { type: "other" };
};

/** One term of a condition: `not person.company`, `person.fields.tier == "gold"`. */
export interface ConditionTerm {
  negated: boolean;
  operand: string;
  comparison?: { equal: boolean; value: string };
}

/** A condition as alternatives (`or`) of terms that must all hold (`and`). */
export const readCondition = (condition: string): ConditionTerm[][] =>
  splitOutside(condition, " or ").map((side) =>
    splitOutside(side, " and ").map((term) => {
      const negated = /^not\s+/u.test(term);
      const rest = negated ? term.replace(/^not\s+/u, "") : term;
      const [left = "", right] = splitOutside(rest, "==");
      if (right !== undefined) {
        return {
          comparison: { equal: true, value: right },
          negated,
          operand: left,
        };
      }
      const [leftDiffers = "", different] = splitOutside(rest, "!=");
      return different === undefined
        ? { negated, operand: rest }
        : {
            comparison: { equal: false, value: different },
            negated,
            operand: leftDiffers,
          };
    })
  );

/** What the templates of a preview read, as the server freezes it when it creates a message. */
export interface PreviewContext {
  namespaces: Readonly<Record<string, unknown>>;
  /** Whether `variables.*` are snippets the AI writes for each person (the step has a prompt). */
  snippets: boolean;
  /** Whether the person is the sample one, whose custom fields show as their names. */
  sample: boolean;
}

/** A stretch of a rendered template. */
export type Piece =
  /** The template's own text: markup in a body, plain text in a subject. */
  | { kind: "text"; text: string }
  /** A value printed: escaped in a body. */
  | { kind: "value"; text: string }
  /** A value the person lacks, printed without a fallback. */
  | { kind: "missing"; path: string }
  /** A snippet the AI writes when it creates the message. */
  | { kind: "snippet"; path: string }
  /** A custom field of the sample person. */
  | { kind: "sample"; path: string };

type Value =
  | { kind: "value"; value: unknown }
  | { kind: "missing"; path: string }
  | { kind: "snippet"; path: string }
  | { kind: "sample"; path: string }
  | { kind: "unknown" };

interface Branch {
  condition: string | null;
  body: Node[];
}

type Node =
  | { type: "text"; text: string }
  | { type: "print"; expression: string; source: string }
  | { type: "if"; branches: Branch[] };

const TOKEN =
  /\{\{-?(?<printed>[\s\S]*?)-?\}\}|\{%-?(?<statement>[\s\S]*?)-?%\}|\{#[\s\S]*?#\}/gu;

/**
 * A `{% … %}` statement read into the tree: `if` opens a block (kept in `open`), `elif` and
 * `else` start its next branch, `endif` closes it; any other statement shows as written. Returns
 * the list the nodes that follow go into.
 */
const readStatement = (
  statement: string,
  whole: string,
  into: Node[],
  open: Branch[][],
  root: Node[]
): Node[] => {
  const { keyword = "", rest = "" } =
    /^\s*(?<keyword>\w+)\s*(?<rest>[\s\S]*?)\s*$/u.exec(statement)?.groups ??
    {};
  const branches = open.at(-1);
  if (keyword === "if") {
    const branch: Branch = { body: [], condition: rest };
    // One list: the node's branches are the ones `elif` and `else` add to while it is open.
    const block = [branch];
    into.push({ branches: block, type: "if" });
    open.push(block);
    return branch.body;
  }
  if ((keyword === "elif" || keyword === "else") && branches) {
    const branch: Branch = {
      body: [],
      condition: keyword === "elif" ? rest : null,
    };
    branches.push(branch);
    return branch.body;
  }
  if (keyword === "endif" && branches) {
    open.pop();
    return open.at(-1)?.at(-1)?.body ?? root;
  }
  into.push({ text: whole, type: "text" });
  return into;
};

/** The tree of a template: text, printed expressions and `if` blocks. */
const parse = (source: string): Node[] => {
  const root: Node[] = [];
  const open: Branch[][] = [];
  let into = root;
  let from = 0;
  for (const match of source.matchAll(TOKEN)) {
    if (match.index > from) {
      into.push({ text: source.slice(from, match.index), type: "text" });
    }
    from = match.index + match[0].length;
    const [whole] = match;
    const { printed, statement } = match.groups ?? {};
    if (printed !== undefined) {
      into.push({ expression: printed.trim(), source: whole, type: "print" });
    } else if (statement !== undefined) {
      into = readStatement(statement, whole, into, open, root);
    }
  }
  if (source.length > from) {
    into.push({ text: source.slice(from), type: "text" });
  }
  return root;
};

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

/** The value at a path of the context; the server leaves absent values out, so `null` is missing. */
const lookup = (path: string, context: PreviewContext): Value => {
  const [namespace = "", ...keys] = path.split(".");
  if (namespace === "variables") {
    return context.snippets && keys.length === 1
      ? { kind: "snippet", path }
      : { kind: "missing", path };
  }
  if (context.sample && namespace === "person" && keys[0] === "fields") {
    return keys.length === 2 ? { kind: "sample", path } : { kind: "unknown" };
  }
  let value: unknown = context.namespaces[namespace];
  for (const key of keys) {
    value = isRecord(value) ? value[key] : undefined;
  }
  return value === undefined || value === null
    ? { kind: "missing", path }
    : { kind: "value", value };
};

/** A value as the template prints it. */
const display = (value: unknown): string => {
  if (typeof value === "string") {
    return value;
  }
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  return value === null || value === undefined ? "" : JSON.stringify(value);
};

/** Each word's first letter in capitals, the rest in lowercase: `ada LOVELACE` → `Ada Lovelace`. */
const title = (text: string): string =>
  text
    .toLowerCase()
    .replaceAll(/(?<=^|\P{L})\p{L}/gu, (letter) => letter.toUpperCase());

const TEXT_FILTERS: Record<string, (text: string) => string> = {
  capitalize: (text) =>
    text.charAt(0).toUpperCase() + text.slice(1).toLowerCase(),
  lower: (text) => text.toLowerCase(),
  title,
  trim: (text) => text.trim(),
  upper: (text) => text.toUpperCase(),
};

/** An operand: a literal or a path. */
const operand = (text: string, context: PreviewContext): Value => {
  const constant = literal(text);
  if (constant) {
    return { kind: "value", value: constant.value };
  }
  return PATH.test(text) ? lookup(text, context) : { kind: "unknown" };
};

/** Sequence filters used by greetings and sender domains, preserving intermediate arrays. */
const sequenceFilter = (value: Value, filter: Filter): Value => {
  if (filter.name === "split") {
    const separator = filter.argument?.trim()
      ? literal(filter.argument)?.value
      : null;
    // Other forms remain visibly unsupported rather than guessing at server semantics.
    if (
      separator !== null &&
      (typeof separator !== "string" || separator === "")
    ) {
      return { kind: "unknown" };
    }
    if (value.kind !== "value") {
      return value;
    }
    const text = display(value.value);
    return {
      kind: "value",
      value:
        separator === null
          ? text.trim().split(/\s+/u).filter(Boolean)
          : text.split(separator),
    };
  }
  if (filter.argument?.trim()) {
    return { kind: "unknown" };
  }
  if (value.kind !== "value") {
    return value;
  }
  let items: readonly unknown[] | null = null;
  if (typeof value.value === "string") {
    items = [...value.value];
  } else if (Array.isArray(value.value)) {
    items = value.value;
  }
  return items === null
    ? { kind: "unknown" }
    : { kind: "value", value: items.at(filter.name === "first" ? 0 : -1) };
};

/** `value` through one filter (`default("there")`, `upper`). */
const filtered = (
  value: Value,
  filter: Filter,
  context: PreviewContext
): Value => {
  if (filter.name === "default") {
    if (
      value.kind !== "missing" &&
      !(value.kind === "value" && value.value === undefined)
    ) {
      return value;
    }
    const [first = ""] = splitOutside(filter.argument ?? "", ",");
    return first === ""
      ? { kind: "value", value: "" }
      : operand(first, context);
  }
  if (["split", "first", "last"].includes(filter.name)) {
    return sequenceFilter(value, filter);
  }
  const change = TEXT_FILTERS[filter.name];
  if (!change) {
    return { kind: "unknown" };
  }
  return value.kind === "value"
    ? { kind: "value", value: change(display(value.value)) }
    : value;
};

/** The value of an expression: an operand and its filters. */
const evaluate = (expression: string, context: PreviewContext): Value => {
  const read = readExpression(expression);
  let value = operand(read.operand, context);
  for (const filter of read.filters) {
    value = filtered(value, filter, context);
  }
  return value;
};

/** Whether a value counts as true: a snippet or a sample field is assumed written. */
const truthy = (value: Value): boolean => {
  if (value.kind === "snippet" || value.kind === "sample") {
    return true;
  }
  if (value.kind !== "value") {
    return false;
  }
  return (
    Boolean(value.value) &&
    !(Array.isArray(value.value) && value.value.length === 0)
  );
};

/** Whether one term of a condition holds for the context. */
const termHolds = (term: ConditionTerm, context: PreviewContext): boolean => {
  const value = evaluate(term.operand, context);
  let result = truthy(value);
  if (term.comparison) {
    const other = evaluate(term.comparison.value, context);
    const equal =
      value.kind === "value" &&
      other.kind === "value" &&
      value.value === other.value;
    result = term.comparison.equal ? equal : !equal;
  }
  return term.negated ? !result : result;
};

/** Whether a condition holds: any of its alternatives, each with all of its terms. */
const holds = (condition: string, context: PreviewContext): boolean =>
  readCondition(condition).some((terms) =>
    terms.every((term) => termHolds(term, context))
  );

/** A printed value as a piece; one the preview cannot evaluate shows as written. */
const piece = (value: Value, source: string): Piece => {
  if (value.kind === "value") {
    return { kind: "value", text: display(value.value) };
  }
  return value.kind === "unknown"
    ? { kind: "text", text: source }
    : { kind: value.kind, path: value.path };
};

const render = (nodes: readonly Node[], context: PreviewContext): Piece[] =>
  nodes.flatMap((node): Piece[] => {
    if (node.type === "text") {
      return [{ kind: "text", text: node.text }];
    }
    if (node.type === "print") {
      return [piece(evaluate(node.expression, context), node.source)];
    }
    const branch = node.branches.find(
      (candidate) =>
        candidate.condition === null || holds(candidate.condition, context)
    );
    return branch ? render(branch.body, context) : [];
  });

/** A template rendered for a preview, as pieces a page can show. */
export const renderTemplate = (
  source: string,
  context: PreviewContext
): Piece[] => render(parse(source), context);

/** Text escaped for HTML content. */
export const escapeHtml = (text: string): string =>
  text.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");

/** The mail frame's class for each kind of filled-in mark. */
const MARKS = { missing: "nb-missing", sample: "nb-sample", snippet: "nb-ai" };

/**
 * A rendered body as HTML for the mail preview: values escaped as the server escapes them, and
 * what a preview fills in marked with the mail frame's `nb-` classes, named by `label`.
 */
export const piecesToHtml = (
  pieces: readonly Piece[],
  label: (path: string) => string
): string =>
  pieces
    .map((part) => {
      if (part.kind === "text") {
        return part.text;
      }
      if (part.kind === "value") {
        return escapeHtml(part.text);
      }
      return `<span class="${MARKS[part.kind]}">${escapeHtml(label(part.path))}</span>`;
    })
    .join("");

/** The values the person lacks in rendered pieces, each once. */
export const missingPaths = (pieces: readonly Piece[]): string[] => [
  ...new Set(
    pieces.flatMap((part) => (part.kind === "missing" ? [part.path] : []))
  ),
];

/** Syntax the browser left untouched, so a partial preview is never presented as complete. */
export const unsupportedTags = (pieces: readonly Piece[]): string[] => [
  ...new Set(
    pieces.flatMap((part) =>
      part.kind === "text"
        ? [...part.text.matchAll(TOKEN)].map(([tag]) => tag)
        : []
    )
  ),
];
