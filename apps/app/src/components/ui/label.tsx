import { cn } from "cn";
import type * as React from "react";

/** A form label: 13px semibold, secondary text, 6px above its control. */
function Label({ className, ...props }: React.ComponentProps<"label">) {
  return (
    // oxlint-disable-next-line jsx-a11y/label-has-associated-control -- callers pass htmlFor
    <label
      className={cn(
        "flex items-center text-sm font-semibold text-fg-2 select-none peer-disabled:cursor-not-allowed peer-disabled:opacity-50",
        className
      )}
      data-slot="label"
      {...props}
    />
  );
}

export { Label };
