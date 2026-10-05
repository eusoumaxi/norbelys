import { Tick02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Checkbox as CheckboxPrimitive } from "@base-ui/react/checkbox";
import { cn } from "cn";

/** `destructive` draws a red box, for confirming something that can't be undone. */
function Checkbox({
  className,
  variant = "default",
  ...props
}: CheckboxPrimitive.Root.Props & { variant?: "default" | "destructive" }) {
  return (
    <CheckboxPrimitive.Root
      className={cn(
        "peer bg-field relative flex size-4 shrink-0 cursor-pointer items-center justify-center rounded-xs border transition-colors outline-none after:absolute after:-inset-2 focus-visible:outline-1 focus-visible:outline-focus data-disabled:cursor-not-allowed data-disabled:opacity-50",
        variant === "default" &&
          "border-line-strong data-checked:border-transparent data-checked:bg-accent data-checked:text-black",
        variant === "destructive" &&
          "border-error-line data-checked:border-transparent data-checked:bg-danger data-checked:text-white",
        className
      )}
      data-slot="checkbox"
      {...props}
    >
      <CheckboxPrimitive.Indicator
        className="grid place-content-center [&>svg]:size-3"
        data-slot="checkbox-indicator"
      >
        <HugeiconsIcon icon={Tick02Icon} strokeWidth={2.5} />
      </CheckboxPrimitive.Indicator>
    </CheckboxPrimitive.Root>
  );
}

export { Checkbox };
