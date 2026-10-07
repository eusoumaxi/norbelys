import type { FieldObject, PersonObject } from "@norbelys/sdk";

import { Dash } from "@/components/data-table";
import { describeValue, draftOf } from "@/features/people/fields";

const matchesPhone = (text: string, query: string): boolean => {
  const digits = query.replaceAll(/[^0-9]/gu, "");
  return (
    digits.length >= 3 &&
    /^[+0-9() .-]+$/u.test(query) &&
    /^[+0-9() .-]+$/u.test(text) &&
    text.replaceAll(/[^0-9]/gu, "").includes(digits)
  );
};

/** Highlight literal text, without treating the search as a regular expression. */
export const SearchMatch = ({
  text,
  query,
}: {
  text: string;
  query: string;
}) => {
  const index = query ? text.toLowerCase().indexOf(query.toLowerCase()) : -1;
  if (index < 0) {
    return matchesPhone(text, query) ? (
      <mark className="bg-go-bg text-go rounded-xs">{text}</mark>
    ) : (
      text
    );
  }
  return (
    <>
      {text.slice(0, index)}
      <mark className="bg-go-bg text-go rounded-xs">
        {text.slice(index, index + query.length)}
      </mark>
      {text.slice(index + query.length)}
    </>
  );
};

/** Explain matches that would otherwise be hidden inside the person's detail. */
export const CustomFieldMatches = ({
  person,
  definitions,
  query,
}: {
  person: PersonObject;
  definitions: readonly FieldObject[];
  query: string;
}) => {
  const needle = query.toLowerCase();
  const matches = Object.entries(person.fields).filter(([, value]) => {
    if (value === null || value === undefined) {
      return false;
    }
    const raw = draftOf(value);
    return (
      raw.toLowerCase().includes(needle) ||
      describeValue(value).toLowerCase().includes(needle) ||
      matchesPhone(raw, query)
    );
  });
  if (matches.length === 0) {
    return <Dash />;
  }
  return (
    <div className="flex max-w-72 flex-col gap-1">
      {matches.map(([key, value]) => {
        const label =
          definitions.find((field) => field.key === key)?.label ?? key;
        const text = describeValue(value);
        return (
          <div
            className="truncate text-xs"
            key={key}
            title={`${label}: ${text}`}
          >
            <span className="text-fg-3">{label}: </span>
            <SearchMatch query={query} text={text} />
          </div>
        );
      })}
    </div>
  );
};
