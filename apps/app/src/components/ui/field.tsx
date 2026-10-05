import { Alert02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { cn } from "cn";

import { Label } from "@/components/ui/label";

function FieldGroup({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      className={cn("flex w-full flex-col gap-4", className)}
      data-slot="field-group"
      {...props}
    />
  );
}

function Field({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      className={cn("flex w-full flex-col gap-1.5", className)}
      data-slot="field"
      role="group"
      {...props}
    />
  );
}

function FieldLabel({
  className,
  ...props
}: React.ComponentProps<typeof Label>) {
  return <Label className={className} data-slot="field-label" {...props} />;
}

/** "Optional" beside a label, in the console's quiet hint style. */
function FieldRequirement({
  requirement,
}: {
  invalid?: boolean;
  requirement: "optional" | "required";
}) {
  return requirement === "optional" ? (
    <span className="text-fg-3 ml-1.5 text-xs font-normal">(optional)</span>
  ) : null;
}

function FieldDescription({
  children,
  className,
  ...props
}: React.ComponentProps<"p">) {
  return (
    <p
      className={cn("text-fg-3 text-xs", className)}
      data-slot="field-description"
      {...props}
    >
      {children}
    </p>
  );
}

function FieldError({
  children,
  className,
  ...props
}: React.ComponentProps<"div">) {
  return (
    <div
      className={cn("text-error-fg flex items-center gap-1.5 text-xs", className)}
      data-slot="field-error"
      role="alert"
      {...props}
    >
      <HugeiconsIcon
        aria-hidden
        className="size-3.5 shrink-0"
        icon={Alert02Icon}
      />
      <span className="flex-1">{children}</span>
    </div>
  );
}

export {
  Field,
  FieldDescription,
  FieldError,
  FieldGroup,
  FieldLabel,
  FieldRequirement,
};
