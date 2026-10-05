import { Collapsible } from "@base-ui/react/collapsible";
import { cn } from "cn";
import { useLayoutEffect, useRef, useState } from "react";
import type { ReactNode } from "react";

/**
 * The space a revealed block keeps from its neighbour, as the parent's flex `gap` (in Tailwind
 * steps). The panel cancels that gap with a negative margin and carries it as padding inside,
 * so the gap opens and closes with the height instead of jumping when the block mounts or
 * leaves.
 */
type Gap = 0 | 2 | 3 | 4 | 5 | 6 | 8;

const TOP: Record<Gap, [string, string]> = {
  0: ["", ""],
  2: ["-mt-2", "pt-2"],
  3: ["-mt-3", "pt-3"],
  4: ["-mt-4", "pt-4"],
  5: ["-mt-5", "pt-5"],
  6: ["-mt-6", "pt-6"],
  8: ["-mt-8", "pt-8"],
};

const BOTTOM: Record<Gap, [string, string]> = {
  0: ["", ""],
  2: ["-mb-2", "pb-2"],
  3: ["-mb-3", "pb-3"],
  4: ["-mb-4", "pb-4"],
  5: ["-mb-5", "pb-5"],
  6: ["-mb-6", "pb-6"],
  8: ["-mb-8", "pb-8"],
};

/**
 * Content that opens and closes in place. It grows from nothing and fades in over 200 ms with
 * the interface's ease-out, and leaves faster (150 ms, ease-in), so what sits below slides
 * instead of jumping. Under reduced motion it shows and hides at once.
 *
 * Base UI's collapsible measures the content, animates the panel's height to it, then lets it
 * follow the content (`auto`), so what changes inside an open block (an error line) still lays
 * out normally. A closed block is unmounted after it has left.
 *
 * In a flex column with a `gap`, pass that gap and the side it sits on (`edge`: `top` when the
 * block follows a sibling, `bottom` when it is the first child): the block then takes no room at
 * all while closed. The children are what shows while the block leaves too, so a caller whose
 * content disappears with the condition keeps it with `useKept`.
 */
export const Reveal = ({
  children,
  className,
  edge = "top",
  gap = 0,
  open,
}: {
  children: ReactNode;
  className?: string;
  edge?: "top" | "bottom";
  gap?: Gap;
  open: boolean;
}) => {
  const [outer, inner] = (edge === "top" ? TOP : BOTTOM)[gap];
  return (
    <Collapsible.Root className="contents" open={open}>
      <Collapsible.Panel
        className={cn(
          "h-(--collapsible-panel-height) overflow-hidden transition-[height,opacity] duration-200 ease-(--nb-ease-out) data-ending-style:h-0 data-ending-style:opacity-0 data-ending-style:duration-150 data-ending-style:ease-(--nb-ease-in) data-starting-style:h-0 data-starting-style:opacity-0 motion-reduce:transition-none",
          outer
        )}
      >
        <div className={cn(inner, className)}>{children}</div>
      </Collapsible.Panel>
    </Collapsible.Root>
  );
};

/**
 * A box whose height follows its content softly, for one thing that becomes another in place (a
 * sender's summary that becomes its form, a note that becomes a text area). The new content
 * shows at once and the box grows or shrinks to it in 200 ms, clipping it meanwhile: one height
 * moving from the old size to the new, where two blocks opening and closing together would
 * overshoot. The first size is taken as it is, so nothing moves when a page loads; under reduced
 * motion the box follows at once.
 */
export const SmoothHeight = ({
  children,
  className,
}: {
  children: ReactNode;
  className?: string;
}) => {
  const inner = useRef<HTMLDivElement>(null);
  const [height, setHeight] = useState<number>();
  useLayoutEffect(() => {
    const element = inner.current;
    if (!element) {
      return;
    }
    const observer = new ResizeObserver(() => setHeight(element.offsetHeight));
    observer.observe(element);
    return () => observer.disconnect();
  }, []);
  return (
    <div
      className="overflow-hidden transition-[height] duration-200 ease-(--nb-ease-out) motion-reduce:transition-none"
      style={height === undefined ? undefined : { height }}
    >
      <div className={className} ref={inner}>
        {children}
      </div>
    </div>
  );
};

/** What enters a `SmoothHeight` after a person's action fades in with it (never on a page's load). */
export const ENTER =
  "animate-in fade-in duration-200 motion-reduce:animate-none";

/**
 * The last `value` that was not `null`: what a closing `Reveal` keeps showing while it leaves,
 * when the condition that opened it and its content go away together. `value` is a key (a
 * string), compared by equality, so it is stored only when it changes.
 */
export const useKept = <T extends string>(value: T | null): T | null => {
  const [kept, setKept] = useState<T | null>(value);
  if (value !== null && value !== kept) {
    setKept(value);
  }
  return value ?? kept;
};
