import { LOCKUP, MARK, markPath, WORDMARK } from "@brand/geometry";
import { cn } from "cn";

/** The mark's one line, from the brand's geometry (never redrawn here). */
const MARK_PATH = markPath();

/**
 * The mark: the n of Norbelys inside an @, one stroked line in the current colour (the accent by
 * default). Small sizes keep the heavier line so the @ still reads at 16 pixels.
 */
export const Logomark = ({
  className,
  stroke = "small",
}: {
  className?: string;
  stroke?: "small" | "regular";
}) => (
  <svg
    aria-hidden
    className={cn("text-accent size-5 shrink-0", className)}
    fill="none"
    viewBox="0 0 64 64"
  >
    <path
      className="nb-line"
      d={MARK_PATH}
      pathLength={1}
      stroke="currentColor"
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth={stroke === "small" ? MARK.smallStroke : MARK.stroke}
    />
  </svg>
);

/**
 * The logo: the mark in the accent and the word in the text colour, on one line. Its height comes
 * from `className` (20px by default); the width follows.
 */
export const Brand = ({
  className,
  stroke = "small",
}: {
  className?: string;
  stroke?: "small" | "regular";
}) => (
  <>
    <svg
      aria-hidden
      className={cn("h-5 w-auto shrink-0", className)}
      viewBox={`0 0 ${LOCKUP.width} ${LOCKUP.height}`}
    >
      <g className="text-accent" transform={`scale(${LOCKUP.markScale})`}>
        <path
          className="nb-line"
          d={MARK_PATH}
          fill="none"
          pathLength={1}
          stroke="currentColor"
          strokeLinecap="round"
          strokeLinejoin="round"
          strokeWidth={stroke === "small" ? MARK.smallStroke : MARK.stroke}
        />
      </g>
      <path
        className="nb-word text-fg"
        d={WORDMARK.d}
        fill="currentColor"
        transform={`translate(${LOCKUP.wordX} 0)`}
      />
    </svg>
    <span className="sr-only">Norbelys</span>
  </>
);

/** Waiting: the @ drawn and undrawn, without end. Announced to screen readers as `label`. */
export const Loader = ({
  className,
  label = "Loading",
}: {
  className?: string;
  label?: string;
}) => (
  <output aria-label={label} className={cn("nb-loader inline-flex", className)}>
    <Logomark className="size-8" stroke="regular" />
  </output>
);

/** A page whose data takes a while (over 300ms) and has no skeleton of its own: the loader, centred. */
export const PageLoader = () => (
  <div className="grid min-h-[50dvh] w-full place-items-center">
    <Loader />
  </div>
);
