import { Link } from "@tanstack/react-router";
import type { LinkProps } from "@tanstack/react-router";
import { cn } from "cn";
import type { ReactNode } from "react";

import { CodeLine } from "@/components/copy";
import { Label } from "@/components/ui/label";

interface SettingsTab {
  label: string;
  link: LinkProps;
  active: boolean;
}

/**
 * The console's settings frame: the title over the full width, then a 200px column of tabs (32px
 * rows, the current one on the selected color) and the panels beside it, 36px apart, at most 800px.
 */
export const SettingsLayout = ({
  children,
  header,
  tabs,
}: {
  children: ReactNode;
  header: ReactNode;
  tabs: SettingsTab[];
}) => (
  <div className="flex flex-col">
    <div className="px-8 pt-5 pb-4">{header}</div>
    <div className="flex flex-col gap-7 md:flex-row">
      <nav
        aria-label="Settings"
        className="shrink-0 px-6 pt-2 md:w-[224px] md:pr-0"
      >
        <ul className="flex gap-1.5 overflow-x-auto md:flex-col">
          {tabs.map((tab) => (
            <li key={tab.label}>
              <Link
                {...tab.link}
                className={cn(
                  "hover:bg-hover focus-visible:outline-focus flex h-8 items-center rounded-sm px-2 text-sm whitespace-nowrap transition-colors outline-none focus-visible:outline-1 md:w-[200px]",
                  tab.active
                    ? "bg-selected hover:bg-selected text-fg font-semibold"
                    : "text-fg-2 hover:text-fg"
                )}
              >
                {tab.label}
              </Link>
            </li>
          ))}
        </ul>
      </nav>
      <div className="flex min-w-0 flex-1 flex-col gap-9 px-6 pt-2 pb-8 md:pr-8 md:pl-0">
        {children}
      </div>
    </div>
  </div>
);

/** One panel of a settings page: a 20px title, a line of explanation, then its controls. */
export const SettingsPanel = ({
  children,
  description,
  title,
}: {
  children?: ReactNode;
  description?: ReactNode;
  title?: ReactNode;
}) => (
  <section className="flex max-w-[800px] min-w-0 flex-col gap-3">
    {title ? <h2 className="text-fg text-2xl font-medium">{title}</h2> : null}
    {description ? (
      <div className="text-fg-2 text-sm">{description}</div>
    ) : null}
    {children}
  </section>
);

/** Values a settings page shows but never edits (an id, a slug), side by side, each copyable. */
export const ReadOnlyFields = ({
  fields,
}: {
  fields: { label: string; value: string }[];
}) => (
  <SettingsPanel>
    <div className="flex flex-col gap-4 sm:flex-row sm:gap-6">
      {fields.map((field) => (
        <div className="flex min-w-0 flex-1 flex-col gap-1.5" key={field.label}>
          <Label>{field.label}</Label>
          <CodeLine
            className="h-8 bg-transparent"
            prefix={null}
            value={field.value}
          />
        </div>
      ))}
    </div>
  </SettingsPanel>
);
