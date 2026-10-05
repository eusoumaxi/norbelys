import { ArrowDown01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { cn } from "cn";
import type { ReactNode } from "react";

import { useStoredFlag } from "@/hooks/use-stored-flag";

export interface Metric {
  label: ReactNode;
  value: ReactNode;
  unit?: ReactNode;
  /** A quieter figure beside the value: a share, a change. */
  note?: ReactNode;
  /** The figure that matters most here (replies), in pink. */
  accent?: boolean;
}

/** Figures in a bordered row: a 12px label over an 18px number, as the console's usage panel. */
export const MetricGroup = ({
  className,
  footer,
  metrics,
}: {
  className?: string;
  footer?: ReactNode;
  metrics: Metric[];
}) => (
  <div
    className={cn(
      "border-line flex flex-col gap-4 rounded-sm border p-6",
      className
    )}
  >
    <dl className="grid grid-cols-2 gap-x-5 gap-y-4 xl:grid-cols-[repeat(auto-fit,minmax(7.5rem,1fr))]">
      {metrics.map((metric, index) => (
        <div className="flex min-w-0 flex-col gap-1" key={index}>
          <dt className="text-fg-2 text-xs leading-3 font-semibold">
            {metric.label}
          </dt>
          <dd className="flex flex-wrap items-baseline gap-1">
            <span
              className={cn(
                "text-xl font-semibold tabular-nums",
                metric.accent ? "text-accent" : "text-fg"
              )}
            >
              {metric.value}
            </span>
            {metric.unit ? (
              <span className="text-fg text-xl font-normal">{metric.unit}</span>
            ) : null}
            {metric.note ? (
              <span className="text-fg-3 text-xs tabular-nums">
                {metric.note}
              </span>
            ) : null}
          </dd>
        </div>
      ))}
    </dl>
    {footer ? <p className="text-fg-2 text-xs">{footer}</p> : null}
  </div>
);

export interface DetailRow {
  label: ReactNode;
  value: ReactNode;
}

/**
 * One section of a details aside: a 14px title that folds its list away. A section folded once
 * stays folded on the next visit (remembered by its title in this browser).
 */
export const DetailSection = ({
  action,
  rows,
  title,
}: {
  action?: ReactNode;
  rows: DetailRow[];
  title: string;
}) => {
  const [folded, setFolded] = useStoredFlag(`nb.details.folded.${title}`);
  const open = !folded;
  return (
    <section className="border-line border-b py-3 last:border-b-0">
      <div className="flex h-7 items-center justify-between gap-2 px-3">
        <h2 className="text-fg text-base font-medium">
          <button
            aria-expanded={open}
            className="focus-visible:outline-focus flex cursor-pointer items-center gap-1 outline-none focus-visible:outline-1"
            onClick={() => setFolded(open)}
            type="button"
          >
            {title}
            <HugeiconsIcon
              className={cn(
                "text-fg-2 size-4 transition-transform duration-200",
                open ? null : "-rotate-90"
              )}
              icon={ArrowDown01Icon}
            />
          </button>
        </h2>
        {action ? (
          <span className="[&_a]:text-fg-3 [&_a]:hover:text-fg text-xs [&_a]:transition-colors">
            {action}
          </span>
        ) : null}
      </div>
      {open ? (
        <dl className="mt-2 flex flex-col gap-2 px-3">
          {rows.map((row, index) => (
            <div className="flex min-w-0 gap-2" key={index}>
              <dt className="text-fg-3 w-[98px] shrink-0 text-xs font-medium">
                {row.label}
              </dt>
              <dd className="text-fg-2 [&_a]:text-link [&_a]:hover:text-link-hover min-w-0 flex-1 text-xs break-words">
                {row.value}
              </dd>
            </div>
          ))}
        </dl>
      ) : null}
    </section>
  );
};

/**
 * Labelled values inside a dialog, as the console's details columns draw them: a 12px label in
 * tertiary text, its value beside it. Rows that are `null` are left out, so a missing field never
 * reads as an empty one.
 */
export const DetailList = ({ rows }: { rows: (DetailRow | null)[] }) => (
  <dl className="grid grid-cols-[minmax(96px,auto)_1fr] gap-x-6 gap-y-2.5">
    {rows
      .filter((row): row is DetailRow => row !== null)
      .map((row, index) => (
        <div className="contents" key={index}>
          <dt className="text-fg-3 pt-px text-xs font-medium">{row.label}</dt>
          <dd className="text-fg-2 [&_a]:text-link [&_a]:hover:text-link-hover min-w-0 text-sm break-words">
            {row.value}
          </dd>
        </div>
      ))}
  </dl>
);

/** The details column beside a page's main column, behind a hairline. */
export const DetailsAside = ({ children }: { children: ReactNode }) => (
  <aside className="border-line w-full shrink-0 lg:w-[276px] lg:border-l">
    {children}
  </aside>
);
