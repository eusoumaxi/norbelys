import { cn } from "cn";
import { Fragment } from "react";
import type { ReactNode } from "react";

import { pathLabel, tagLook } from "@/features/messages/merge-tags";
import type { FieldName } from "@/features/messages/merge-tags";
import { TOKEN } from "@/features/messages/template-editor";
import { templateParts } from "@/features/messages/templates";
import type { Piece } from "@/features/messages/templates";

/** A step's place in the sequence, as a small round number; the selected step's is in the accent. */
export const StepNumber = ({
  position,
  selected = false,
}: {
  position: number;
  selected?: boolean;
}) => (
  <span
    className={cn(
      "flex h-5 min-w-5 shrink-0 items-center justify-center rounded-full border px-1 text-xs font-semibold tabular-nums transition-colors duration-120",
      selected ? "border-accent text-accent" : "border-line-strong text-fg-2"
    )}
  >
    {position}
  </span>
);

/** The mark of something the last refused save named, beside where it is. */
export const ProblemDot = ({ label }: { label: string }) => (
  <span className="flex shrink-0 items-center">
    <span aria-hidden className="bg-error size-1.5 rounded-full" />
    <span className="sr-only">{label}</span>
  </span>
);

/**
 * A value an email is filled in with, named as the editor names it: the same token, so a person
 * recognises it everywhere. `missing` marks a value the person of a preview lacks.
 */
const Token = ({
  children,
  kind,
}: {
  children: ReactNode;
  kind: "value" | "ai" | "condition" | "code" | "missing";
}) => (
  <span
    className={cn(
      TOKEN.base,
      kind === "missing" ? "bg-error-bg text-error-fg" : TOKEN[kind]
    )}
  >
    {children}
  </span>
);

/**
 * A template as a person reads it, on one line: its text, each tag as its token
 * (`{{ person.given_name | default("there") }}` shows as "First name").
 */
export const TemplateText = ({
  fields,
  text,
}: {
  fields: readonly FieldName[];
  text: string;
}) =>
  templateParts(text).map((part) => {
    if (!part.tag) {
      return <Fragment key={part.from}>{part.text}</Fragment>;
    }
    const look = tagLook(part.text, fields);
    return (
      <Token key={part.from} kind={look.kind}>
        {look.label}
      </Token>
    );
  });

/** How a preview marks what it could not fill with a value. */
const PIECE_KINDS = {
  missing: "missing",
  sample: "value",
  snippet: "ai",
} as const;

/** A rendered template's pieces as text: values filled in, what the preview could not fill as tokens. */
export const PieceText = ({
  fields,
  pieces,
}: {
  fields: readonly FieldName[];
  pieces: readonly Piece[];
}) =>
  pieces.map((piece, index) => {
    // Pieces have no identity of their own; their order never changes between renders.
    const key = `${index}:${piece.kind}`;
    if (piece.kind === "text" || piece.kind === "value") {
      return <Fragment key={key}>{piece.text}</Fragment>;
    }
    return (
      <Token key={key} kind={PIECE_KINDS[piece.kind]}>
        {pathLabel(piece.path, fields)}
      </Token>
    );
  });

/**
 * The classes that let an element ease in when it appears after the page loaded (a step opened,
 * a variant switched, the preview shown): a fade and a 4px rise in 200 ms, nothing under reduced
 * motion. Empty on the page's first render, which appears without motion.
 */
export const enter = (moving: boolean): string | null =>
  moving
    ? "transition-[opacity,translate] duration-200 ease-(--nb-ease-out) starting:translate-y-1 starting:opacity-0 motion-reduce:transition-none"
    : null;
