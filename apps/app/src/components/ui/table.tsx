import { cn } from "cn";
import type * as React from "react";

/**
 * The console's tables. `shell` draws them in a bordered, 3px-rounded box with a head on the
 * chrome color (the overview's tables); without it the table sits on the page between hairlines
 * (the lists). Heads are 46px in 13px semibold, rows 46px in 13px.
 */
function Table({
  className,
  shell = false,
  ...props
}: React.ComponentProps<"table"> & { shell?: boolean }) {
  return (
    <div
      className={cn(
        "scrollbar-thin min-w-0 w-full overflow-x-auto",
        shell ? "border-line bg-surface rounded-sm border" : null
      )}
      data-shell={shell ? "" : undefined}
      data-slot="table-container"
    >
      <table
        className={cn("w-full border-collapse text-left text-sm", className)}
        data-slot="table"
        {...props}
      />
    </div>
  );
}

function TableHeader({ className, ...props }: React.ComponentProps<"thead">) {
  return (
    <thead
      className={cn(
        "[[data-shell]_&]:bg-chrome [&_tr]:border-t-0",
        className
      )}
      data-slot="table-header"
      {...props}
    />
  );
}

function TableBody({ className, ...props }: React.ComponentProps<"tbody">) {
  return (
    <tbody
      className={cn(
        "[[data-shell]_&_tr:last-child]:border-b-0 [&_tr]:border-line [&_tr]:border-t",
        className
      )}
      data-slot="table-body"
      {...props}
    />
  );
}

function TableRow({ className, ...props }: React.ComponentProps<"tr">) {
  return (
    <tr
      className={cn(
        "border-line border-b transition-colors [[data-slot=table-body]_&]:hover:bg-chrome/60 data-[state=selected]:bg-hover",
        className
      )}
      data-slot="table-row"
      {...props}
    />
  );
}

function TableHead({ className, ...props }: React.ComponentProps<"th">) {
  return (
    <th
      className={cn(
        "text-fg h-[46px] px-4 py-3.5 align-middle text-sm leading-4 font-semibold whitespace-nowrap",
        className
      )}
      data-slot="table-head"
      {...props}
    />
  );
}

function TableCell({ className, ...props }: React.ComponentProps<"td">) {
  return (
    <td
      className={cn(
        "text-fg h-[46px] px-4 py-2 align-middle text-sm leading-[18px]",
        className
      )}
      data-slot="table-cell"
      {...props}
    />
  );
}

export { Table, TableBody, TableCell, TableHead, TableHeader, TableRow };
