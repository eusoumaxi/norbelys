import type { FieldType } from "@norbelys/sdk";

import { humanize } from "@/lib/format";

/** The custom field types, in the order a form offers them, with what each one stores. */
export const FIELD_TYPES: {
  description: string;
  label: string;
  value: FieldType;
}[] = [
  {
    description: "Free text, up to 1,000 characters.",
    label: "Text",
    value: "text",
  },
  { description: "Any number.", label: "Number", value: "number" },
  { description: "Yes or no.", label: "Yes / no", value: "boolean" },
  {
    description: "One of a list of options you set.",
    label: "Choice",
    value: "enum",
  },
  { description: "A calendar date.", label: "Date", value: "date" },
];

/** `enum` → `Choice`; a type this dashboard does not know yet shows as its own words. */
export const fieldTypeLabel = (type: string): string =>
  FIELD_TYPES.find((entry) => entry.value === type)?.label ?? humanize(type);

/**
 * A stored value as a form control holds it: text, `""` when the person has none, booleans as
 * `"true"` or `"false"`.
 */
export const draftOf = (value: unknown): string => {
  if (value === null || value === undefined) {
    return "";
  }
  if (typeof value === "string") {
    return value;
  }
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  return JSON.stringify(value);
};

/**
 * What a control's text stores under a field of `type`: `null` clears the value. A number that
 * does not read as one is sent as written, so the API's own message says what is wrong.
 */
export const valueOf = (type: string, text: string): unknown => {
  const trimmed = text.trim();
  if (trimmed === "") {
    return null;
  }
  if (type === "number") {
    const number = Number(trimmed);
    return Number.isFinite(number) ? number : trimmed;
  }
  if (type === "boolean") {
    return trimmed === "true";
  }
  return trimmed;
};

/** A stored value in words: `Yes`/`No` for a boolean, the value itself otherwise. */
export const describeValue = (value: unknown): string => {
  if (typeof value === "boolean") {
    return value ? "Yes" : "No";
  }
  return draftOf(value);
};

/**
 * A field key suggested from a label, in the shape the API accepts: a lowercase letter, then
 * lowercase letters, digits or underscores, at most 64 characters. Accents are dropped
 * (`Teléfono` is `telefono`, not `tele_fono`).
 */
export const keyFromLabel = (label: string): string => {
  const key = label
    .normalize("NFKD")
    .replaceAll(/\p{M}/gu, "")
    .toLowerCase()
    .replaceAll(/[^a-z0-9]+/gu, "_")
    .replaceAll(/^_+|_+$/gu, "")
    .replace(/^[^a-z]+/u, "");
  return key.slice(0, 64);
};
