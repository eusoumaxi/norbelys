import { Delete02Icon, PencilEdit02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Link } from "@tanstack/react-router";
import type { LinkProps } from "@tanstack/react-router";
import { cn } from "cn";
import { useState } from "react";
import type { ReactNode } from "react";

import { Slash } from "@/components/slash";
import { Button } from "@/components/ui/button";

/** Where a detail page belongs, and the way back to it: "Campaigns" above a campaign's name. */
interface BackTo {
  label: string;
  link: LinkProps;
}

interface PageHeaderProps {
  title: ReactNode;
  actions?: ReactNode;
  /** The list a detail page belongs to: "Campaigns / Founders outreach", the first part a link. */
  back?: BackTo;
  /** The line under the title: a status, or a sentence. */
  subtitle?: ReactNode;
  /** Tighter spacing under the header, for dense pages such as an overview. */
  compact?: boolean;
}

/**
 * The title row: a 24px title, the actions on the right, and an optional line under it. On a
 * detail page the list it belongs to leads the title in grey, joined by the top bar's slash
 * ("Campaigns / Founders outreach"): where the page is and the way back, in the line the title
 * already takes, as the top bar reads "Norbelys / Acme".
 */
export const PageHeader = ({
  actions,
  back,
  compact = false,
  subtitle,
  title,
}: PageHeaderProps) => (
  <header
    className={cn(
      "flex flex-wrap items-center gap-x-4 gap-y-3",
      compact ? "mb-3" : "mb-5"
    )}
  >
    {/* On a phone the list sits above the title, so a long name keeps the line to itself. */}
    {back ? (
      <Link
        {...back.link}
        className="text-fg-3 hover:text-fg -mb-2 basis-full text-sm transition-colors sm:hidden"
      >
        {back.label}
      </Link>
    ) : null}
    {/* The title keeps its line; on a narrow page the actions move under it. */}
    <div className="flex min-w-0 flex-[1_1_240px] items-center gap-1.5">
      {back ? (
        <span className="hidden shrink-0 items-center gap-1.5 sm:flex">
          <Link
            {...back.link}
            className="text-fg-3 hover:text-fg focus-visible:outline-focus rounded-xs text-3xl font-semibold transition-colors outline-none focus-visible:outline-1"
          >
            {back.label}
          </Link>
          <Slash className="h-7 w-4" />
        </span>
      ) : null}
      <h1 className="text-fg min-w-0 text-3xl font-semibold break-words">
        {title}
      </h1>
    </div>
    {actions ? (
      <div className="flex flex-wrap items-center gap-2">{actions}</div>
    ) : null}
    {subtitle ? (
      <div className="text-fg-2 -mt-1 min-w-0 basis-full text-sm">
        {subtitle}
      </div>
    ) : null}
  </header>
);

/** Whether a dialog is open, and how it asks to open or close: spread on the dialog. */
interface DialogState {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/**
 * A detail page's header actions for people who may change it: Edit, then Delete, each opening
 * the dialog its render function draws with the state it is given.
 */
export const EditDeleteActions = ({
  renderDelete,
  renderEdit,
}: {
  renderDelete: (dialog: DialogState) => ReactNode;
  renderEdit: (dialog: DialogState) => ReactNode;
}) => {
  const [editing, setEditing] = useState(false);
  const [deleting, setDeleting] = useState(false);
  return (
    <>
      <Button onClick={() => setEditing(true)} variant="secondary">
        <HugeiconsIcon icon={PencilEdit02Icon} />
        Edit
      </Button>
      <Button onClick={() => setDeleting(true)} variant="danger-secondary">
        <HugeiconsIcon icon={Delete02Icon} />
        Delete
      </Button>
      {renderEdit({ onOpenChange: setEditing, open: editing })}
      {renderDelete({ onOpenChange: setDeleting, open: deleting })}
    </>
  );
};

/** The page's column inside the content panel: 20px above, 32px at the sides and below. */
export const PageBody = ({
  children,
  className,
}: {
  children: ReactNode;
  className?: string;
}) => (
  <div
    className={cn(
      "mx-auto w-full max-w-[1600px] min-w-0 px-4 pt-5 pb-8 sm:px-8",
      className
    )}
  >
    {children}
  </div>
);

/** A titled block of a page: an 18px heading over its content. */
export const Section = ({
  actions,
  children,
  className,
  title,
}: {
  actions?: ReactNode;
  children: ReactNode;
  className?: string;
  title: ReactNode;
}) => (
  <section className={cn("flex flex-col gap-2", className)}>
    <div className="flex min-h-7 items-center justify-between gap-3">
      <h2 className="text-fg text-xl font-semibold">{title}</h2>
      {actions ? (
        <div className="flex items-center gap-2">{actions}</div>
      ) : null}
    </div>
    {children}
  </section>
);
