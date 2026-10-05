/**
 * The details an email can be personalised with, in a salesperson's words: what each one is
 * called, what it reads (`person.given_name`), and what is printed for a person who lacks it.
 * The editor shows a tag by these names ("First name, or “there”"); the Personalize menu inserts
 * them; a preview names what it could not fill.
 */
import type { FieldObject } from "@norbelys/sdk";

import {
  literal,
  PATH,
  readCondition,
  readExpression,
  readTag,
} from "@/features/messages/templates";
import type { ConditionTerm } from "@/features/messages/templates";
import { humanize } from "@/lib/format";

/** A value a template can print, as the Personalize menu offers it. */
export interface MergeTag {
  /** What a person reads: "First name". */
  label: string;
  /** What the template reads: `person.given_name`. */
  path: string;
  /** What is printed when the person lacks the value; absent when everyone has it. */
  fallback?: string;
  /** What is inserted: the path, with its fallback. */
  tag: string;
}

/** The fields of a custom field definition a tag needs. */
export type FieldName = Pick<FieldObject, "key" | "label">;

/**
 * `{{ path }}`, or `{{ path | default("…") }}` when a person may lack the value: printing a
 * missing value refuses the message, so the fallback is printed instead.
 */
export const tagFor = (path: string, fallback?: string): string =>
  fallback === undefined
    ? `{{ ${path} }}`
    : `{{ ${path} | default(${JSON.stringify(fallback)}) }}`;

const mergeTag = (label: string, path: string, fallback?: string): MergeTag =>
  fallback === undefined
    ? { label, path, tag: tagFor(path) }
    : { fallback, label, path, tag: tagFor(path, fallback) };

/** The person's own details. */
export const PERSON_TAGS: MergeTag[] = [
  mergeTag("First name", "person.given_name", "there"),
  mergeTag("Last name", "person.family_name", ""),
  mergeTag("Company", "person.company", "your company"),
  mergeTag("Email address", "person.email"),
];

/** The mailbox identity that sends the message. */
export const SENDER_TAGS: MergeTag[] = [
  mergeTag("Sender name", "sender.name", ""),
  mergeTag("Sender email address", "sender.email"),
];

/** A custom field of the workspace, read as `person.fields.<key>`. */
export const fieldTag = (field: FieldName): MergeTag =>
  mergeTag(field.label, `person.fields.${field.key}`, "");

/** The snippets offered before a step reads any: what AI personalisation is most often for. */
const SNIPPET_PRESETS = new Map([
  ["opener", "Opening line"],
  ["ps", "P.S."],
]);

/**
 * An AI snippet, read as `variables.<name>`: the AI writes it for each person from the step's
 * prompt, and its fallback (nothing) is sent when the AI cannot.
 */
const snippetTag = (name: string): MergeTag =>
  mergeTag(
    SNIPPET_PRESETS.get(name) ?? humanize(name),
    `variables.${name}`,
    ""
  );

/** The snippets the menu offers a step: those its templates read, then the presets. */
export const snippetTags = (used: readonly string[]): MergeTag[] =>
  [...new Set([...used, ...SNIPPET_PRESETS.keys()])].map(snippetTag);

/** What a person calls the value at `path`: "First name", a custom field's label, a snippet's name. */
export const pathLabel = (
  path: string,
  fields: readonly FieldName[]
): string => {
  const known = [...PERSON_TAGS, ...SENDER_TAGS].find(
    (candidate) => candidate.path === path
  );
  if (known) {
    return known.label;
  }
  const field = /^person\.fields\.(?<key>\w+)$/u.exec(path)?.groups?.key;
  if (field) {
    return (
      fields.find((entry) => entry.key === field)?.label ?? humanize(field)
    );
  }
  const snippet = /^variables\.(?<name>\w+)$/u.exec(path)?.groups?.name;
  if (snippet) {
    return snippetTag(snippet).label;
  }
  const other: Record<string, string> = {
    "campaign.name": "Campaign name",
    "step.name": "Step name",
    "step.position": "Step number",
    unsubscribe_url: "Unsubscribe link",
  };
  return other[path] ?? path;
};

/** How a tag shows in the editor: a value, an AI snippet, a condition, or template code. */
export interface TagLook {
  kind: "value" | "ai" | "condition" | "code";
  /** What it says: "First name", "If Company is set". */
  label: string;
  /** What is printed when the person lacks the value ("" prints nothing); absent without one. */
  fallback?: string;
}

/** One term of a condition in words: "Company is set", "Tier is “Gold”". */
const termWords = (term: ConditionTerm, fields: readonly FieldName[]) => {
  const { operand } = readExpression(term.operand);
  const name = PATH.test(operand) ? pathLabel(operand, fields) : operand;
  if (!term.comparison) {
    return term.negated ? `${name} is empty` : `${name} is set`;
  }
  const value = literal(term.comparison.value);
  const shown =
    typeof value?.value === "string"
      ? `“${value.value}”`
      : term.comparison.value;
  const equal = term.comparison.equal !== term.negated;
  return equal ? `${name} is ${shown}` : `${name} isn't ${shown}`;
};

/** A condition in words: its alternatives joined by "or", their terms by "and". */
const conditionWords = (condition: string, fields: readonly FieldName[]) =>
  readCondition(condition)
    .map((terms) => terms.map((term) => termWords(term, fields)).join(" and "))
    .join(" or ");

/** A printed tag's look: the value it names and its fallback, or the code itself. */
const printedLook = (
  expression: string,
  fields: readonly FieldName[]
): TagLook => {
  const { filters, operand } = readExpression(expression);
  if (!PATH.test(operand)) {
    return { kind: "code", label: expression };
  }
  const look: TagLook = {
    kind: operand.startsWith("variables.") ? "ai" : "value",
    label: pathLabel(operand, fields),
  };
  const fallback = filters.find((filter) => filter.name === "default");
  if (!fallback) {
    return look;
  }
  const [first = ""] = (fallback.argument ?? "").split(",");
  const value = literal(first.trim());
  if (value) {
    return {
      ...look,
      fallback: value.value === null ? "" : String(value.value),
    };
  }
  return PATH.test(first.trim())
    ? { ...look, fallback: pathLabel(first.trim(), fields) }
    : look;
};

/**
 * How the editor shows a tag: `{{ person.given_name | default("there") }}` is the value
 * "First name" with the fallback "there"; `{% if person.company %}` is "If Company is set";
 * anything else is shown as the code it is.
 */
export const tagLook = (
  source: string,
  fields: readonly FieldName[]
): TagLook => {
  const read = readTag(source);
  if (read.type === "print") {
    return printedLook(read.expression, fields);
  }
  if (read.type === "statement") {
    const words: Record<string, string> = {
      elif: `Or if ${conditionWords(read.rest, fields)}`,
      else: "Otherwise",
      endif: "End of if",
      if: `If ${conditionWords(read.rest, fields)}`,
    };
    const label = words[read.keyword];
    if (label) {
      return { kind: "condition", label };
    }
  }
  return { kind: "code", label: source };
};

/**
 * A printed tag with its fallback set to `fallback`: the `default` filter changed (or added),
 * every other filter kept. `null` for a tag that prints no path.
 */
export const withFallback = (tag: string, fallback: string): string | null => {
  const read = readTag(tag);
  if (read.type !== "print") {
    return null;
  }
  const { filters, operand } = readExpression(read.expression);
  if (!PATH.test(operand)) {
    return null;
  }
  const value = JSON.stringify(fallback);
  const kept = filters.map((filter) =>
    filter.name === "default" ? { ...filter, argument: value } : filter
  );
  const all = kept.some((filter) => filter.name === "default")
    ? kept
    : [...kept, { argument: value, name: "default" }];
  const written = all.map((filter) =>
    filter.argument === undefined
      ? filter.name
      : `${filter.name}(${filter.argument})`
  );
  return `{{ ${[operand, ...written].join(" | ")} }}`;
};

/** What a condition can test about a value. */
export type Test = "set" | "empty" | "is" | "isNot";

/**
 * The template of "show this only to some people": `shown` for the people `path` and `test`
 * (and `value`) select, `otherwise` for everyone else. The texts are taken as written.
 */
export const conditionTemplate = ({
  otherwise,
  path,
  shown,
  test,
  value,
}: {
  otherwise: string;
  path: string;
  /** What the people the condition selects read. */
  shown: string;
  test: Test;
  value: string;
}): string => {
  const quoted = JSON.stringify(value);
  const conditions: Record<Test, string> = {
    empty: `not ${path}`,
    is: `${path} | default("") == ${quoted}`,
    isNot: `${path} | default("") != ${quoted}`,
    set: path,
  };
  const rest = otherwise ? `{% else %}${otherwise}` : "";
  return `{% if ${conditions[test]} %}${shown}${rest}{% endif %}`;
};
