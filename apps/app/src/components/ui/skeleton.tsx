import { cn } from "cn";

/** The console's loading shimmer: a soft gradient sweeping across a 3px-rounded block. */
function Skeleton({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      aria-hidden
      className={cn("skeleton rounded-sm", className)}
      data-slot="skeleton"
      {...props}
    />
  );
}

export { Skeleton };
