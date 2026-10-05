import { Button as ButtonPrimitive } from "@base-ui/react/button";
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "cn";

/**
 * The console's buttons: 32px (`m`) or 28px (`s`), 3px corners, 13px semibold labels, a 16px icon
 * 6px from the label. Hover and press turn green, the brand's accent.
 */
const buttonVariants = cva(
  "inline-flex shrink-0 cursor-pointer items-center justify-center gap-1.5 rounded-sm border text-sm font-semibold whitespace-nowrap transition-colors duration-200 ease-in-out outline-none select-none focus-visible:outline-1 focus-visible:outline-offset-1 focus-visible:outline-focus disabled:cursor-not-allowed [&_svg]:pointer-events-none [&_svg]:size-4 [&_svg]:shrink-0",
  {
    variants: {
      variant: {
        primary:
          "border-transparent bg-button text-button-fg hover:bg-button-hover active:bg-button-active disabled:bg-selected disabled:text-fg-4",
        secondary:
          "border-line-strong bg-transparent text-secondary-fg hover:border-go-line hover:bg-go-bg hover:text-go active:bg-go-bg-strong active:text-go aria-expanded:border-go-line aria-expanded:text-go disabled:border-line-strong disabled:bg-transparent disabled:text-fg-4",
        tertiary:
          "border-transparent bg-transparent text-secondary-fg hover:bg-go-bg hover:text-go active:bg-go-bg-strong active:text-go aria-expanded:bg-go-bg aria-expanded:text-go disabled:bg-transparent disabled:text-fg-4",
        connect:
          "border-go-line bg-go-bg text-go hover:bg-go-bg-strong active:bg-go-bg-strong",
        danger:
          "border-transparent bg-danger text-white hover:bg-danger-hover disabled:bg-selected disabled:text-fg-4",
        "danger-secondary":
          "border-error-line bg-transparent text-error-fg hover:bg-error-bg disabled:border-line disabled:text-fg-4",
        link: "h-auto border-0 px-0 font-semibold text-link hover:text-link-hover",
      },
      size: {
        s: "h-7 px-3",
        m: "h-8 px-4",
        "icon-s": "size-7 px-0 [&_svg]:text-icon",
        "icon-m": "size-8 px-0 [&_svg]:text-icon",
      },
    },
    defaultVariants: {
      variant: "secondary",
      size: "m",
    },
  }
);

function Button({
  className,
  variant = "secondary",
  size = "m",
  ...props
}: ButtonPrimitive.Props & VariantProps<typeof buttonVariants>) {
  return (
    <ButtonPrimitive
      className={cn(buttonVariants({ variant, size }), className)}
      data-slot="button"
      {...props}
    />
  );
}

export { Button, buttonVariants };
