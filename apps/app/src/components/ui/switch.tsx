import { Switch as SwitchPrimitive } from "@base-ui/react/switch";
import { cn } from "cn";

/** The console's toggle: a 28x16 track, gray when off and the accent when on. */
function Switch({ className, ...props }: SwitchPrimitive.Root.Props) {
  return (
    <SwitchPrimitive.Root
      className={cn(
        "bg-fg-4 data-checked:bg-go-line relative inline-flex h-4 w-7 shrink-0 cursor-pointer items-center rounded-full transition-colors outline-none focus-visible:outline-1 focus-visible:outline-offset-2 focus-visible:outline-focus data-disabled:cursor-not-allowed data-disabled:opacity-50",
        className
      )}
      data-slot="switch"
      {...props}
    >
      <SwitchPrimitive.Thumb className="bg-icon data-checked:bg-white block size-3 translate-x-0.5 rounded-full transition-transform duration-150 data-checked:translate-x-[14px]" />
    </SwitchPrimitive.Root>
  );
}

export { Switch };
