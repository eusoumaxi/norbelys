import { Input as InputPrimitive } from "@base-ui/react/input";
import { cn } from "cn";
import type * as React from "react";

/** The chrome every text control shares: 32px, 3px corners, a hairline that strengthens on hover. */
const fieldClasses =
  "w-full min-w-0 rounded-sm border border-field-line bg-field text-sm text-fg transition-colors duration-200 outline-none placeholder:text-fg-3 hover:border-line-strong focus-visible:border-focus focus-visible:outline-none disabled:cursor-not-allowed disabled:border-line disabled:bg-chrome disabled:text-fg-4 aria-invalid:border-error-line";

function Input({ className, type, ...props }: React.ComponentProps<"input">) {
  return (
    <InputPrimitive
      className={cn("flex h-8 px-3", fieldClasses, className)}
      data-slot="input"
      type={type}
      {...props}
    />
  );
}

export { fieldClasses, Input };
