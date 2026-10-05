import { Cancel01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { cn } from "cn";
import { useState } from "react";
import type { ClipboardEvent, KeyboardEvent } from "react";

import { Badge } from "@/components/ui/badge";

/** Values added from typed or pasted text: one per line, trimmed, none twice, at most `max`. */
const merged = (values: string[], text: string, max: number): string[] => {
  const next = [...values];
  for (const line of text.split(/\r?\n/u)) {
    const value = line.trim();
    if (value && !next.includes(value)) {
      next.push(value);
    }
  }
  return next.slice(0, max);
};

/**
 * A short list of values typed one at a time: Enter adds the text as a chip, Backspace in the
 * empty input removes the last one, and a pasted list adds a value per line. Commas are kept
 * inside a value, because options and company names may hold them. Used for an enum field's
 * options and a segment condition's `in` list.
 */
export const ValuesInput = ({
  id,
  invalid = false,
  label,
  max = 100,
  onChange,
  placeholder = "Type a value, then press Enter",
  values,
}: {
  id?: string;
  invalid?: boolean;
  /** The accessible name when no visible label points at `id`. */
  label?: string;
  max?: number;
  onChange: (values: string[]) => void;
  placeholder?: string;
  values: string[];
}) => {
  const [text, setText] = useState("");
  const commit = (raw: string) => {
    const next = merged(values, raw, max);
    if (next.length !== values.length) {
      onChange(next);
    }
    setText("");
  };
  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "Enter") {
      event.preventDefault();
      commit(text);
    } else if (event.key === "Backspace" && text === "" && values.length > 0) {
      onChange(values.slice(0, -1));
    }
  };
  const onPaste = (event: ClipboardEvent<HTMLInputElement>) => {
    const pasted = event.clipboardData.getData("text");
    if (/\r?\n/u.test(pasted)) {
      event.preventDefault();
      commit(`${text}${pasted}`);
    }
  };
  return (
    <div
      className={cn(
        "border-field-line bg-field hover:border-line-strong focus-within:border-focus flex min-h-8 w-full min-w-0 flex-wrap items-center gap-1 rounded-sm border px-1.5 py-1 transition-colors",
        invalid ? "border-error-line" : null
      )}
    >
      {values.map((value) => (
        <Badge className="max-w-full gap-1 pr-1" key={value}>
          <span className="truncate">{value}</span>
          <button
            aria-label={`Remove ${value}`}
            className="text-fg-3 hover:text-fg focus-visible:outline-focus flex size-4 shrink-0 cursor-pointer items-center justify-center rounded-xs outline-none focus-visible:outline-1"
            onClick={() => onChange(values.filter((item) => item !== value))}
            type="button"
          >
            <HugeiconsIcon className="size-3" icon={Cancel01Icon} />
          </button>
        </Badge>
      ))}
      <input
        aria-invalid={invalid}
        aria-label={label}
        className="text-fg placeholder:text-fg-3 h-6 min-w-28 flex-1 bg-transparent px-1.5 text-sm outline-none disabled:cursor-not-allowed"
        disabled={values.length >= max}
        id={id}
        onBlur={() => commit(text)}
        onChange={(event) => setText(event.target.value)}
        onKeyDown={onKeyDown}
        onPaste={onPaste}
        placeholder={values.length === 0 ? placeholder : undefined}
        value={text}
      />
    </div>
  );
};
