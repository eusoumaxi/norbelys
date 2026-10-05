import { Avatar as AvatarPrimitive } from "@base-ui/react/avatar";
import { cn } from "cn";

/** An 18px round avatar; without an image, the initial on the brand's accent. */
function Avatar({ className, ...props }: AvatarPrimitive.Root.Props) {
  return (
    <AvatarPrimitive.Root
      className={cn(
        "relative flex size-[18px] shrink-0 overflow-hidden rounded-full select-none",
        className
      )}
      data-slot="avatar"
      {...props}
    />
  );
}


function AvatarFallback({
  className,
  ...props
}: AvatarPrimitive.Fallback.Props) {
  return (
    <AvatarPrimitive.Fallback
      className={cn(
        "bg-accent flex size-full items-center justify-center text-[8px] leading-none font-semibold text-[#0d0d0d]",
        className
      )}
      data-slot="avatar-fallback"
      {...props}
    />
  );
}

export { Avatar, AvatarFallback };
