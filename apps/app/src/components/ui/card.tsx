import { cn } from "cn";
import type * as React from "react";

/**
 * shadcn's card, drawn as the console's panels: black, a hairline, 3px corners. The header is a
 * 44px row (10px by 16px) with a 16px title; the content has 16px at the sides and below.
 */
function Card({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      className={cn(
        "border-line bg-surface text-fg flex flex-col rounded-sm border",
        className
      )}
      data-slot="card"
      {...props}
    />
  );
}

function CardHeader({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      className={cn(
        "flex min-h-11 items-center justify-between gap-3 px-4 py-2.5",
        className
      )}
      data-slot="card-header"
      {...props}
    />
  );
}

function CardTitle({ className, ...props }: React.ComponentProps<"h2">) {
  return (
    <h2
      className={cn("text-fg text-lg font-medium", className)}
      data-slot="card-title"
      {...props}
    />
  );
}

function CardDescription({ className, ...props }: React.ComponentProps<"p">) {
  return (
    <p
      className={cn("text-fg-2 text-sm", className)}
      data-slot="card-description"
      {...props}
    />
  );
}


function CardContent({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      className={cn(
        "px-4 pb-4 [[data-slot=card]>&:first-child]:pt-4",
        className
      )}
      data-slot="card-content"
      {...props}
    />
  );
}

function CardFooter({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      className={cn("border-line flex items-center border-t px-4 py-3", className)}
      data-slot="card-footer"
      {...props}
    />
  );
}

export {
  Card,
  CardContent,
  CardDescription,
  CardFooter,
  CardHeader,
  CardTitle,
};
