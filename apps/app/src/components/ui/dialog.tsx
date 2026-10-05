import { Dialog as DialogPrimitive } from "@base-ui/react/dialog";
import { Cancel01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { cn } from "cn";
import type * as React from "react";

function Dialog({ ...props }: DialogPrimitive.Root.Props) {
  return <DialogPrimitive.Root data-slot="dialog" {...props} />;
}


function DialogClose({ ...props }: DialogPrimitive.Close.Props) {
  return <DialogPrimitive.Close data-slot="dialog-close" {...props} />;
}

/**
 * The console's modal: black, a hairline, 3px corners, over a dimmed page; a header with a 20px
 * title and a 40px close button, the body, and a footer above a hairline with the actions.
 */
function DialogContent({
  className,
  children,
  ...props
}: DialogPrimitive.Popup.Props) {
  return (
    <DialogPrimitive.Portal>
      <DialogPrimitive.Backdrop
        className="bg-overlay fixed inset-0 z-50 transition-opacity duration-200 ease-(--nb-ease-out) data-ending-style:opacity-0 data-ending-style:duration-150 data-starting-style:opacity-0"
        data-slot="dialog-overlay"
      />
      <DialogPrimitive.Popup
        className={cn(
          "border-line bg-surface text-fg shadow-dialog fixed top-1/2 left-1/2 z-50 flex max-h-[calc(100dvh-48px)] w-[calc(100%-32px)] max-w-[560px] -translate-x-1/2 -translate-y-1/2 flex-col overflow-hidden rounded-sm border transition-[opacity,translate,scale] duration-240 ease-(--nb-ease-out) outline-none data-ending-style:opacity-0 data-ending-style:duration-150 data-ending-style:ease-(--nb-ease-in) data-starting-style:translate-y-[calc(-50%+4px)] data-starting-style:scale-[0.98] data-starting-style:opacity-0",
          className
        )}
        data-slot="dialog-content"
        {...props}
      >
        {children}
        <DialogPrimitive.Close
          aria-label="Close"
          className="text-fg hover:bg-hover absolute top-6 right-6 flex size-10 cursor-pointer items-center justify-center rounded-xl border border-white/20 transition-colors outline-none focus-visible:outline-1 focus-visible:outline-focus light:border-black/15"
          data-slot="dialog-close"
        >
          <HugeiconsIcon className="size-5" icon={Cancel01Icon} />
        </DialogPrimitive.Close>
      </DialogPrimitive.Popup>
    </DialogPrimitive.Portal>
  );
}

function DialogHeader({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      className={cn(
        "flex min-h-16 flex-col justify-center gap-1 px-6 pt-6 pr-20 pb-3",
        className
      )}
      data-slot="dialog-header"
      {...props}
    />
  );
}

function DialogBody({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      className={cn(
        "flex grow flex-col gap-4 overflow-y-auto px-6 pt-3 pb-6",
        className
      )}
      data-slot="dialog-body"
      {...props}
    />
  );
}

function DialogFooter({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      className={cn(
        "border-line flex items-center justify-end gap-2 border-t p-6 [&_[data-slot=button]]:shadow-button",
        className
      )}
      data-slot="dialog-footer"
      {...props}
    />
  );
}

function DialogTitle({ className, ...props }: DialogPrimitive.Title.Props) {
  return (
    <DialogPrimitive.Title
      className={cn("text-fg text-2xl font-medium", className)}
      data-slot="dialog-title"
      {...props}
    />
  );
}

function DialogDescription({
  className,
  ...props
}: DialogPrimitive.Description.Props) {
  return (
    <DialogPrimitive.Description
      className={cn("text-fg-2 text-sm", className)}
      data-slot="dialog-description"
      {...props}
    />
  );
}

export {
  Dialog,
  DialogBody,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
};
