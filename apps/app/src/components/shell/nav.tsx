import { Collapsible } from "@base-ui/react/collapsible";
import {
  Activity01Icon,
  Analytics01Icon,
  ArrowDown01Icon,
  CheckmarkBadge01Icon,
  CodeSquareIcon,
  DashboardSpeed01Icon,
  FileImportIcon,
  Mailbox01Icon,
  Note01Icon,
  ServerStack01Icon,
  DashboardSquare01Icon,
  FilterIcon,
  Globe02Icon,
  InboxIcon,
  Key01Icon,
  Mail01Icon,
  MailAccount01Icon,
  Megaphone01Icon,
  Settings02Icon,
  UnavailableIcon,
  UserGroupIcon,
  UserIcon,
  UserMultiple02Icon,
  WebhookIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { IconSvgElement } from "@hugeicons/react";
import { Link, useRouterState } from "@tanstack/react-router";
import { cn } from "cn";
import { useEffect } from "react";
import type { ReactNode } from "react";

import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { useStoredFlag } from "@/hooks/use-stored-flag";

/** A destination inside a workspace, relative to `/w/$slug`. */
interface NavLink {
  label: string;
  icon: IconSvgElement;
  /** The path under the workspace, `""` for its overview. */
  path: string;
}

interface NavGroup {
  label: string;
  icon: IconSvgElement;
  children: NavLink[];
}

type NavEntry = NavLink | NavGroup;

/** Workspace administration stays at the foot of navigation, away from daily work. */
const workspaceSettings: NavLink = {
  icon: Settings02Icon,
  label: "Settings",
  path: "settings",
};

/** The workspace's sections, in the order the sidebar lists them. */
const workspaceNav: NavEntry[][] = [
  [
    { icon: DashboardSquare01Icon, label: "Overview", path: "" },
    { icon: Analytics01Icon, label: "Analytics", path: "analytics" },
    { icon: Megaphone01Icon, label: "Campaigns", path: "campaigns" },
    { icon: Mail01Icon, label: "Messages", path: "messages" },
    { icon: InboxIcon, label: "Inbox", path: "inbox" },
  ],
  [
    {
      children: [
        { icon: UserIcon, label: "People", path: "people" },
        { icon: UserGroupIcon, label: "Groups", path: "groups" },
        { icon: FilterIcon, label: "Segments", path: "segments" },
        { icon: Note01Icon, label: "Fields", path: "fields" },
        { icon: FileImportIcon, label: "Imports", path: "imports" },
        { icon: UnavailableIcon, label: "Suppressions", path: "suppressions" },
      ],
      icon: UserMultiple02Icon,
      label: "Audience",
    },
    {
      children: [
        { icon: MailAccount01Icon, label: "Mailboxes", path: "mailboxes" },
        { icon: Globe02Icon, label: "Sending domains", path: "domains" },
        { icon: DashboardSpeed01Icon, label: "Sending limits", path: "quotas" },
      ],
      icon: Mailbox01Icon,
      label: "Sending",
    },
    {
      children: [
        { icon: Key01Icon, label: "API keys", path: "api-keys" },
        { icon: WebhookIcon, label: "Webhooks", path: "webhooks" },
        { icon: Activity01Icon, label: "Events", path: "events" },
        { icon: ServerStack01Icon, label: "Delivery logs", path: "logs" },
        {
          icon: CheckmarkBadge01Icon,
          label: "Address check",
          path: "preflight",
        },
      ],
      icon: CodeSquareIcon,
      label: "Developers",
    },
  ],
];

/** Every link of the workspace navigation, flattened (the search menu lists them). */
export const workspaceLinks: NavLink[] = [
  ...workspaceNav
    .flat()
    .flatMap((entry) => ("children" in entry ? entry.children : [entry])),
  workspaceSettings,
];

/** The route of a workspace page by its path under `/w/$slug`; `""` is the overview. */
export const workspaceRoute = (path: string) =>
  // Every path of `workspaceNav` is a route under `/w/$slug`.
  (path ? `/w/$slug/${path}` : "/w/$slug") as "/w/$slug";

// The current page's row is styled from the link's own `data-status="active"`, not `activeProps`:
// the router appends active classes to the base ones, and the stylesheet's order (not the class
// list's) then decides between `text-fg-2` and `text-fg`, so the current row stayed grey.
const itemClasses =
  "group/item flex min-h-7 w-full cursor-pointer items-center gap-3 rounded-sm px-3.5 py-1 text-sm text-fg-2 transition-colors duration-200 outline-none hover:bg-hover focus-visible:outline-1 focus-visible:outline-focus [&>svg]:size-4 [&>svg]:shrink-0 [&>svg]:text-fg-2 data-[status=active]:bg-selected data-[status=active]:font-semibold data-[status=active]:text-fg data-[status=active]:hover:bg-selected data-[status=active]:[&>svg]:text-accent";

/** One 28px row: a 16px icon, 12px, the label. Collapsed, only the icon, with a tooltip. */
const ItemFrame = ({
  collapsed,
  label,
  render,
}: {
  collapsed: boolean;
  label: string;
  render: ReactNode;
}) =>
  collapsed ? (
    <Tooltip>
      <TooltipTrigger render={render as React.ReactElement} />
      <TooltipContent side="right">{label}</TooltipContent>
    </Tooltip>
  ) : (
    render
  );

const NavItemLink = ({
  collapsed = false,
  link,
  onNavigate,
  slug,
}: {
  collapsed?: boolean;
  link: NavLink;
  onNavigate?: () => void;
  slug: string;
}) => (
  <li>
    <ItemFrame
      collapsed={collapsed}
      label={link.label}
      render={
        <Link
          activeOptions={{ exact: link.path === "" }}
          className={cn(itemClasses, collapsed ? "justify-center px-0" : null)}
          onClick={onNavigate}
          params={{ slug }}
          to={workspaceRoute(link.path)}
        >
          <HugeiconsIcon icon={link.icon} />
          {collapsed ? (
            <span className="sr-only">{link.label}</span>
          ) : (
            <span className="truncate">{link.label}</span>
          )}
        </Link>
      }
    />
  </li>
);

/** A folding group that remembers its state and opens when navigation enters one of its pages. */
const NavItemGroup = ({
  collapsed,
  group,
  onNavigate,
  slug,
}: {
  collapsed: boolean;
  group: NavGroup;
  onNavigate?: () => void;
  slug: string;
}) => {
  const pathname = useRouterState({
    select: (state) => state.location.pathname,
  });
  const current = group.children.some((link) =>
    pathname.startsWith(`/w/${slug}/${link.path}`)
  );
  const [open, setOpen] = useStoredFlag(
    `nb.sidebar.${slug}.${group.label}`,
    current
  );
  useEffect(() => {
    if (current) {
      setOpen(true);
    }
  }, [current, setOpen]);
  if (collapsed) {
    return (
      <>
        {group.children.map((link) => (
          <NavItemLink
            collapsed
            key={link.path}
            link={link}
            onNavigate={onNavigate}
            slug={slug}
          />
        ))}
      </>
    );
  }
  return (
    <Collapsible.Root open={open} onOpenChange={setOpen} render={<li />}>
      <Collapsible.Trigger className={itemClasses} type="button">
        <HugeiconsIcon icon={group.icon} />
        <span className="flex-1 truncate text-left">{group.label}</span>
        <HugeiconsIcon
          className={cn(
            "text-icon! size-4 transition-transform duration-200 motion-reduce:transition-none",
            open ? null : "-rotate-90"
          )}
          icon={ArrowDown01Icon}
        />
      </Collapsible.Trigger>
      <Collapsible.Panel className="h-(--collapsible-panel-height) overflow-hidden transition-[height] duration-200 ease-(--nb-ease-out) data-ending-style:h-0 data-starting-style:h-0 motion-reduce:transition-none">
        <ul className="mt-1 flex flex-col gap-1 pl-4">
          {group.children.map((link) => (
            <NavItemLink
              key={link.path}
              link={link}
              onNavigate={onNavigate}
              slug={slug}
            />
          ))}
        </ul>
      </Collapsible.Panel>
    </Collapsible.Root>
  );
};

/** The workspace's sections, 24px apart, rows 4px apart. */
export const WorkspaceNav = ({
  collapsed = false,
  onNavigate,
  slug,
}: {
  collapsed?: boolean;
  onNavigate?: () => void;
  slug: string;
}) => (
  <div className="flex flex-col gap-6">
    {workspaceNav.map((section, index) => (
      <ul className="flex flex-col gap-1" key={index}>
        {section.map((entry) =>
          "children" in entry ? (
            <NavItemGroup
              collapsed={collapsed}
              group={entry}
              key={`${slug}.${entry.label}`}
              onNavigate={onNavigate}
              slug={slug}
            />
          ) : (
            <NavItemLink
              collapsed={collapsed}
              key={entry.path}
              link={entry}
              onNavigate={onNavigate}
              slug={slug}
            />
          )
        )}
      </ul>
    ))}
  </div>
);

/** Workspace settings in the navigation footer, also available when the sidebar is collapsed. */
export const WorkspaceSettings = ({
  collapsed = false,
  onNavigate,
  slug,
}: {
  collapsed?: boolean;
  onNavigate?: () => void;
  slug: string;
}) => (
  <nav aria-label="Workspace administration">
    <ul>
      <NavItemLink
        collapsed={collapsed}
        link={workspaceSettings}
        onNavigate={onNavigate}
        slug={slug}
      />
    </ul>
  </nav>
);

/** A plain row (a link elsewhere, or an action) drawn like the navigation's. */
export const NavRow = ({
  collapsed = false,
  icon,
  label,
  render,
}: {
  collapsed?: boolean;
  icon: IconSvgElement;
  label: string;
  /** The element: an `<a>` or a `<button>`, given the row's classes and content. */
  render: (props: { className: string; children: ReactNode }) => ReactNode;
}) => (
  <ItemFrame
    collapsed={collapsed}
    label={label}
    render={render({
      children: (
        <>
          <HugeiconsIcon icon={icon} />
          {collapsed ? (
            <span className="sr-only">{label}</span>
          ) : (
            <span className="truncate text-left">{label}</span>
          )}
        </>
      ),
      className: cn(itemClasses, collapsed ? "justify-center px-0" : null),
    })}
  />
);
