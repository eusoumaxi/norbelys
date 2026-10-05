import { cn } from "cn";

/**
 * The thin slash that joins a place to what is inside it: "Norbelys / Acme" in the top bar,
 * "Campaigns / Founders outreach" in a detail page's title. Drawn rather than typed, so it keeps
 * one weight and leans the same way at every size.
 */
export const Slash = ({ className }: { className?: string }) => (
  <svg
    aria-hidden
    className={cn("text-line-strong h-5 w-3 shrink-0", className)}
    fill="none"
    viewBox="0 0 12 20"
  >
    <path
      d="M8.5 2.5 3.5 17.5"
      stroke="currentColor"
      strokeLinecap="round"
      strokeWidth="1.25"
      vectorEffect="non-scaling-stroke"
    />
  </svg>
);
