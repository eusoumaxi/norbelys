import { Tabs as TabsPrimitive } from "@base-ui/react/tabs";
import { Link } from "@tanstack/react-router";
import type { LinkProps } from "@tanstack/react-router";
import { cn } from "cn";

import { formatCount } from "@/lib/format";

/**
 * The console's tabs: 13px semibold labels 24px apart over a hairline; the others grey, the current
 * one bright and underlined 2px in the accent (Base UI marks it `data-active`, a route link
 * `data-status="active"`). `Tabs` keeps its state; `TabLinks` are routes (a detail page's sections).
 */
const listClasses =
  "border-line flex gap-6 overflow-x-auto overflow-y-hidden border-b";

const tabClasses =
  "text-fg-3 -mb-px flex h-[34px] shrink-0 cursor-pointer items-start border-b-2 border-transparent pt-0 pb-3 text-sm font-semibold whitespace-nowrap transition-colors outline-none hover:text-fg focus-visible:outline-1 focus-visible:outline-focus data-active:border-accent data-active:text-fg data-[status=active]:border-accent data-[status=active]:text-fg";

function Tabs({ ...props }: TabsPrimitive.Root.Props) {
  return <TabsPrimitive.Root data-slot="tabs" {...props} />;
}

function TabsList({ className, ...props }: TabsPrimitive.List.Props) {
  return (
    <TabsPrimitive.List
      className={cn(listClasses, className)}
      data-slot="tabs-list"
      {...props}
    />
  );
}

function TabsTab({ className, ...props }: TabsPrimitive.Tab.Props) {
  return (
    <TabsPrimitive.Tab
      className={cn(tabClasses, className)}
      data-slot="tabs-tab"
      {...props}
    />
  );
}

function TabsPanel({ className, ...props }: TabsPrimitive.Panel.Props) {
  return (
    <TabsPrimitive.Panel
      className={cn("pt-5 outline-none", className)}
      data-slot="tabs-panel"
      {...props}
    />
  );
}

export interface TabLink {
  label: string;
  link: LinkProps;
  /** How many things the section holds, quiet beside its label. */
  count?: number;
  /** Matches the current route only exactly (the first tab, at the detail's own path). */
  exact?: boolean;
}

/** Tabs that are routes: each section of a detail page has its own address. */
function TabLinks({
  className,
  tabs,
}: {
  className?: string;
  tabs: TabLink[];
}) {
  return (
    <nav aria-label="Sections" className={cn(listClasses, className)}>
      {tabs.map((tab) => (
        <Link
          {...tab.link}
          activeOptions={{ exact: tab.exact ?? false, includeSearch: false }}
          className={tabClasses}
          key={tab.label}
        >
          {tab.label}
          {tab.count === undefined ? null : (
            <span className="text-fg-3 ml-1.5 font-normal tabular-nums">
              {formatCount(tab.count)}
            </span>
          )}
        </Link>
      ))}
    </nav>
  );
}

export { TabLinks, Tabs, TabsList, TabsPanel, TabsTab };
