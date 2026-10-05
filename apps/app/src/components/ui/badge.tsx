import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "cn";
import type * as React from "react";

/**
 * Two small marks, both 20px and 12px text:
 *
 * - a tag (`dot` false): a label beside something ("Winner", a tag, "Disabled"), drawn as a 2px
 *   radius outline with no fill, so a row of them stays quiet;
 * - a status (`dot` true): a 6px dot and a word, with no frame at all. The dot carries the tone;
 *   the word stays grey for routine states and takes the tone only for what needs a look
 *   (warning, error) or what matters most (a reply, in pink). Eight identical "Active" rows then
 *   read as a column of grey words, and the one "Bounced" stands out.
 */
const badgeVariants = cva(
  "inline-flex h-5 shrink-0 items-center gap-1.5 rounded-xs border px-1.5 text-xs font-medium whitespace-nowrap",
  {
    variants: {
      tone: {
        neutral: "border-line text-fg-2",
        success: "border-success-line/50 text-success-fg",
        info: "border-info-line/60 text-info",
        warning: "border-warning-line/60 text-warning",
        error: "border-error-line/60 text-error-fg",
        accent: "border-accent/50 text-accent",
        beta: "border-beta-line text-fg",
        muted: "border-line text-fg-3",
      },
    },
    defaultVariants: {
      tone: "neutral",
    },
  }
);

type Tone = NonNullable<VariantProps<typeof badgeVariants>["tone"]>;

const dotColors: Record<Tone, string> = {
  accent: "bg-accent",
  beta: "bg-beta-line",
  error: "bg-error",
  info: "bg-info",
  muted: "bg-fg-4",
  neutral: "bg-fg-3",
  success: "bg-success",
  warning: "bg-warning-line",
};

/**
 * A status's 6px dot in its tone. `live` adds the brand's slow ping, for a state that is happening
 * right now (a campaign sending); reduced motion stills it.
 */
function StatusDot({
  className,
  live = false,
  tone = "neutral",
}: {
  className?: string;
  live?: boolean;
  tone?: Tone;
}) {
  return (
    <span
      aria-hidden
      className={cn("relative inline-flex size-1.5 shrink-0", className)}
    >
      {live ? (
        <span
          className={cn("nb-live absolute inset-0 rounded-full", dotColors[tone])}
        />
      ) : null}
      <span className={cn("relative size-1.5 rounded-full", dotColors[tone])} />
    </span>
  );
}

/** A status's word: grey unless the state asks for a look (warning, error) or matters (accent). */
const statusText: Record<Tone, string> = {
  accent: "text-accent",
  beta: "text-fg-2",
  error: "text-error-fg",
  info: "text-fg-2",
  muted: "text-fg-3",
  neutral: "text-fg-2",
  success: "text-fg-2",
  warning: "text-warning",
};

function Badge({
  className,
  tone = "neutral",
  dot = false,
  children,
  ...props
}: React.ComponentProps<"span"> & { tone?: Tone; dot?: boolean }) {
  if (dot) {
    return (
      <span
        className={cn(
          "inline-flex h-5 shrink-0 items-center gap-1.5 text-sm whitespace-nowrap",
          statusText[tone],
          className
        )}
        data-slot="status"
        {...props}
      >
        <StatusDot tone={tone} />
        {children}
      </span>
    );
  }
  return (
    <span
      className={cn(badgeVariants({ tone }), className)}
      data-slot="badge"
      {...props}
    >
      {children}
    </span>
  );
}

export { Badge, StatusDot, statusText, type Tone };
