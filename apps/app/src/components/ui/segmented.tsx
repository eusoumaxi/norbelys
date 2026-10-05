import { cn } from "cn";

export interface SegmentedOption<T extends string> {
  label: string;
  value: T;
}

/**
 * The console's segmented control: joined 32px buttons with hairlines, the chosen one on the
 * selected color (a time range, a view). A radio group for assistive technology.
 */
export const Segmented = <T extends string>({
  className,
  label,
  onChange,
  options,
  value,
}: {
  className?: string;
  label: string;
  onChange: (value: T) => void;
  options: SegmentedOption<T>[];
  value: T;
}) => (
  <div
    aria-label={label}
    className={cn("flex shrink-0", className)}
    role="radiogroup"
  >
    {options.map((option) => (
      <button
        aria-checked={option.value === value}
        className={cn(
          "border-line -ml-px flex h-8 cursor-pointer items-center border px-3 text-sm transition-colors outline-none first:ml-0 first:rounded-l-sm last:rounded-r-sm focus-visible:z-10 focus-visible:outline-1 focus-visible:outline-focus",
          option.value === value
            ? "bg-selected text-fg"
            : "bg-surface text-fg-3 hover:bg-hover hover:text-fg"
        )}
        key={option.value}
        onClick={() => onChange(option.value)}
        role="radio"
        type="button"
      >
        {option.label}
      </button>
    ))}
  </div>
);
