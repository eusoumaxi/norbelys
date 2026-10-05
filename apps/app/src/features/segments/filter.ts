import type { Condition, FieldObject, Operator } from "@norbelys/sdk";

import { describeValue, draftOf } from "@/features/people/fields";
import { formatDateTime, toLocalInput } from "@/lib/format";

const isOperandList = (value: unknown): value is readonly unknown[] =>
  Array.isArray(value);

/**
 * What a condition's field holds, which decides its operators and its value: an address and its
 * domain compare in lowercase, a name, the company and a text field exactly, `created_at` as an
 * instant, and each other custom field by its type.
 */
export type Kind =
  | "address"
  | "boolean"
  | "choice"
  | "date"
  | "domain"
  | "instant"
  | "number"
  | "text";

const ORDERED: Operator[] = [
  "equals",
  "not_equals",
  "in",
  "gt",
  "gte",
  "lt",
  "lte",
  "exists",
  "not_exists",
];

/** The operators the API accepts for each kind of field, in the order a picker offers them. */
export const OPERATORS: Record<Kind, Operator[]> = {
  address: ["equals", "not_equals", "in", "starts_with"],
  boolean: ["equals", "not_equals", "in", "exists", "not_exists"],
  choice: ["equals", "not_equals", "in", "exists", "not_exists"],
  date: ORDERED,
  domain: ["equals", "not_equals", "in"],
  instant: ["gt", "gte", "lt", "lte"],
  number: ORDERED,
  text: ["equals", "not_equals", "in", "starts_with", "exists", "not_exists"],
};

/** A field a condition can name: a person's own attribute or a custom field (`fields.<key>`). */
export interface Subject {
  field: string;
  kind: Kind;
  label: string;
  /** An enum field's options. */
  options: string[];
  /** True for a custom field. */
  custom: boolean;
}

const BUILT_IN: Subject[] = [
  {
    custom: false,
    field: "email",
    kind: "address",
    label: "Email",
    options: [],
  },
  {
    custom: false,
    field: "email_domain",
    kind: "domain",
    label: "Email domain",
    options: [],
  },
  {
    custom: false,
    field: "given_name",
    kind: "text",
    label: "First name",
    options: [],
  },
  {
    custom: false,
    field: "family_name",
    kind: "text",
    label: "Last name",
    options: [],
  },
  {
    custom: false,
    field: "company",
    kind: "text",
    label: "Company",
    options: [],
  },
  {
    custom: false,
    field: "created_at",
    kind: "instant",
    label: "Created",
    options: [],
  },
];

const KIND_OF_TYPE: Record<string, Kind> = {
  boolean: "boolean",
  date: "date",
  enum: "choice",
  number: "number",
  text: "text",
};

/** Every field a condition can name: the built-in attributes, then the custom fields. */
export const subjectsOf = (fields: readonly FieldObject[]): Subject[] => [
  ...BUILT_IN,
  ...fields.flatMap((field) => {
    const kind = KIND_OF_TYPE[field.type];
    return kind
      ? [
          {
            custom: true,
            field: `fields.${field.key}`,
            kind,
            label: field.label,
            options: field.options,
          },
        ]
      : [];
  }),
];

/** The subject a condition names; a field this dashboard does not know reads as text. */
export const subjectOf = (subjects: Subject[], field: string): Subject =>
  subjects.find((subject) => subject.field === field) ?? {
    custom: field.startsWith("fields."),
    field,
    kind: "text",
    label: field,
    options: [],
  };

const WORDS: Record<Operator, string> = {
  equals: "is",
  exists: "is set",
  gt: "is greater than",
  gte: "is at least",
  in: "is one of",
  lt: "is less than",
  lte: "is at most",
  not_equals: "is not",
  not_exists: "is not set",
  starts_with: "starts with",
};

const TIME_WORDS: Partial<Record<Operator, string>> = {
  gt: "is after",
  gte: "is on or after",
  lt: "is before",
  lte: "is on or before",
};

/** An operator in words, for a field of `kind`: `gt` is "is after" for a date. */
export const operatorLabel = (kind: Kind, operator: Operator): string => {
  if (kind === "instant" || kind === "date") {
    return TIME_WORDS[operator] ?? WORDS[operator];
  }
  return WORDS[operator];
};

/** `exists` and `not_exists` take no value. */
export const takesValue = (operator: Operator): boolean =>
  operator !== "exists" && operator !== "not_exists";

/** One condition as the builder edits it: the value as text, and `in`'s list as texts. */
export interface DraftCondition {
  /** A stable React key; never sent. */
  key: string;
  field: string;
  operator: Operator;
  value: string;
  values: string[];
}

/** A new condition on `subject`, with the first operator it accepts. */
export const blankCondition = (subject: Subject): DraftCondition => ({
  field: subject.field,
  key: crypto.randomUUID(),
  operator: OPERATORS[subject.kind][0] ?? "equals",
  value: "",
  values: [],
});

/** A stored operand as the builder's text: an instant in the browser's time zone, as typed. */
const textOf = (kind: Kind, value: unknown): string =>
  kind === "instant" &&
  typeof value === "string" &&
  !Number.isNaN(Date.parse(value))
    ? toLocalInput(value)
    : draftOf(value);

/**
 * The operand a text stands for, typed for the field: a number, a boolean, an RFC 3339 instant.
 * Text that does not read as its type is sent as written, so the API says what is wrong with it.
 */
const operandOf = (kind: Kind, text: string): unknown => {
  const trimmed = text.trim();
  if (kind === "number") {
    const number = Number(trimmed);
    return trimmed && Number.isFinite(number) ? number : trimmed;
  }
  if (kind === "boolean" && (trimmed === "true" || trimmed === "false")) {
    return trimmed === "true";
  }
  if (kind === "instant") {
    const time = Date.parse(trimmed);
    return Number.isNaN(time) ? trimmed : new Date(time).toISOString();
  }
  return trimmed;
};

/** A stored condition as the builder edits it. */
export const draftCondition = (
  condition: Condition,
  subjects: Subject[]
): DraftCondition => {
  const { kind } = subjectOf(subjects, condition.field);
  const list = isOperandList(condition.value) ? condition.value : null;
  return {
    field: condition.field,
    key: crypto.randomUUID(),
    operator: condition.operator,
    value: list ? "" : textOf(kind, condition.value),
    values: list ? list.map((value) => textOf(kind, value)) : [],
  };
};

/** The condition the API takes from a draft: no value for a presence test, a list for `in`. */
export const conditionOf = (
  draft: DraftCondition,
  subjects: Subject[]
): Condition => {
  const { kind } = subjectOf(subjects, draft.field);
  if (!takesValue(draft.operator)) {
    return { field: draft.field, operator: draft.operator };
  }
  if (draft.operator === "in") {
    return {
      field: draft.field,
      operator: draft.operator,
      value: draft.values.map((value) => operandOf(kind, value)),
    };
  }
  return {
    field: draft.field,
    operator: draft.operator,
    value: operandOf(kind, draft.value),
  };
};

/** A stored operand in words: a date and time in the reader's zone, yes or no, else as written. */
const describeOperand = (kind: Kind, value: unknown): string => {
  if (kind === "instant" && typeof value === "string") {
    return formatDateTime(value);
  }
  return describeValue(value);
};

/** A condition's operands in words: none, one, or each of `in`'s list. */
export const operandsOf = (condition: Condition, kind: Kind): string[] => {
  if (!takesValue(condition.operator)) {
    return [];
  }
  const list = isOperandList(condition.value)
    ? condition.value
    : [condition.value];
  return list.map((value) => describeOperand(kind, value));
};
