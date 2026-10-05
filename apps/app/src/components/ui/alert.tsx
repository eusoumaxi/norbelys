import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "cn";
import type * as React from "react";

/**
 * The console's banners: a tinted panel with a matching hairline, a 20px icon, a 16px title and the
 * body under it.
 */
const alertVariants = cva(
  "grid w-full grid-cols-[auto_1fr] items-start gap-x-2 rounded-sm border p-4 text-sm text-fg [&>svg]:mt-0.5 [&>svg]:size-5 [&>svg]:shrink-0",
  {
    variants: {
      variant: {
        info: "border-info-line bg-info-bg [&>svg]:text-info",
        success: "border-success-line bg-success-bg [&>svg]:text-success-fg",
        warning: "border-warning-line bg-warning-bg [&>svg]:text-warning",
        error: "border-error-line bg-error-bg [&>svg]:text-error",
        neutral: "border-line bg-chrome [&>svg]:text-icon",
      },
    },
    defaultVariants: {
      variant: "neutral",
    },
  }
);

function Alert({
  className,
  variant,
  ...props
}: React.ComponentProps<"div"> & VariantProps<typeof alertVariants>) {
  return (
    <div
      className={cn(alertVariants({ variant }), className)}
      data-slot="alert"
      role="alert"
      {...props}
    />
  );
}

function AlertTitle({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      className={cn("col-start-2 min-w-0 text-lg font-medium", className)}
      data-slot="alert-title"
      {...props}
    />
  );
}

function AlertDescription({
  className,
  ...props
}: React.ComponentProps<"div">) {
  return (
    <div
      className={cn(
        "col-start-2 min-w-0 [[data-slot=alert-title]+&]:mt-2 [&_a]:font-semibold [&_a]:text-link [&_a]:hover:text-link-hover",
        className
      )}
      data-slot="alert-description"
      {...props}
    />
  );
}

export { Alert, AlertDescription, AlertTitle };
