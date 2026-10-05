import { Drawer } from "@base-ui/react/drawer";
import {
  BookOpen01Icon,
  Building03Icon,
  Menu01Icon,
  SidebarLeftIcon,
  TestTube01Icon,
  UserIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Link } from "@tanstack/react-router";
import { cn } from "cn";
import { useState } from "react";
import type { ReactNode } from "react";

import { Brand, Logomark } from "@/components/brand";
import { CommandMenu } from "@/components/shell/command-menu";
import {
  NavRow,
  WorkspaceNav,
  WorkspaceSettings,
} from "@/components/shell/nav";
import { UserMenu } from "@/components/shell/user-menu";
import { WorkspaceSwitcher } from "@/components/shell/workspace-switcher";
import { Button } from "@/components/ui/button";
import { useStoredFlag } from "@/hooks/use-stored-flag";
import { DOCS } from "@/lib/links";
import type { Workspace } from "@/lib/workspace";

const COLLAPSED = "nb.sidebar.collapsed";

/** The links of the pages outside any workspace. */
const AccountNav = ({
  collapsed = false,
  onNavigate,
}: {
  collapsed?: boolean;
  onNavigate?: () => void;
}) => (
  <ul className="flex flex-col gap-1">
    {[
      {
        exact: true,
        icon: Building03Icon,
        label: "Workspaces",
        to: "/workspaces" as const,
      },
      {
        exact: false,
        icon: UserIcon,
        label: "Settings",
        to: "/account" as const,
      },
    ].map((link) => (
      <li key={link.to}>
        <NavRow
          collapsed={collapsed}
          icon={link.icon}
          label={link.label}
          render={({ children, className }) => (
            <Link
              activeOptions={{ exact: link.exact }}
              className={className}
              onClick={onNavigate}
              to={link.to}
            >
              {children}
            </Link>
          )}
        />
      </li>
    ))}
  </ul>
);

/** Everything the sidebar holds above its pinned footer. */
const SidebarSections = ({
  collapsed = false,
  onNavigate,
  workspace,
}: {
  collapsed?: boolean;
  onNavigate?: () => void;
  workspace?: Workspace;
}) =>
  workspace ? (
    <WorkspaceNav
      collapsed={collapsed}
      onNavigate={onNavigate}
      slug={workspace.slug}
    />
  ) : (
    <AccountNav collapsed={collapsed} onNavigate={onNavigate} />
  );

/**
 * The strip along the top of a test-mode workspace's pages: nothing it sends leaves Norbelys,
 * which is worth seeing on every page and needs no more than one line.
 */
const TestModeBar = () => (
  <div className="border-warning-line/40 bg-warning-bg/40 text-warning flex items-center justify-center gap-2 border-b px-4 py-1.5 text-xs">
    <HugeiconsIcon className="size-3.5" icon={TestTube01Icon} />
    <span>
      <span className="font-semibold">Test mode.</span> Nothing this workspace
      sends leaves Norbelys: messages stop at a test transport.
    </span>
  </div>
);

/**
 * The dashboard's frame, as the console draws it: a 56px top bar and a 220px sidebar on the chrome
 * color, and the page in a black panel inset 4px from the window's right and bottom edges, lit by
 * a faint pink glow at its top. On phones the sidebar opens as a sheet from the top bar.
 */
export const AppShell = ({
  children,
  workspace,
}: {
  children: ReactNode;
  workspace?: Workspace;
}) => {
  const [collapsed, setCollapsed] = useStoredFlag(COLLAPSED);
  const [menuOpen, setMenuOpen] = useState(false);
  const toggle = () => setCollapsed(!collapsed);

  return (
    <div className="bg-chrome flex h-dvh flex-col overflow-hidden">
      <header className="flex h-14 shrink-0 items-center justify-between gap-3 pr-3 pl-4 md:pr-6">
        <div className="flex min-w-0 items-center gap-1.5">
          <button
            aria-label="Open navigation"
            className="text-icon hover:bg-hover flex size-8 cursor-pointer items-center justify-center rounded-sm md:hidden"
            onClick={() => setMenuOpen(true)}
            type="button"
          >
            <HugeiconsIcon className="size-4" icon={Menu01Icon} />
          </button>
          <Link
            aria-label="Norbelys home"
            className="focus-visible:outline-focus flex shrink-0 items-center rounded-xs pr-1 outline-none focus-visible:outline-1"
            to="/"
          >
            <Logomark className="size-6 sm:hidden" />
            <span className="hidden sm:flex">
              <Brand />
            </span>
          </Link>
          {workspace ? <WorkspaceSwitcher workspace={workspace} /> : null}
        </div>
        <div className="flex shrink-0 items-center gap-2">
          <CommandMenu workspace={workspace} />
          <Button
            className="hidden md:inline-flex"
            render={
              <a
                aria-label="Documentation"
                href={DOCS}
                rel="noreferrer"
                target="_blank"
              />
            }
            size="s"
            variant="secondary"
          >
            <HugeiconsIcon icon={BookOpen01Icon} />
            Docs
          </Button>
          <UserMenu />
        </div>
      </header>

      <div className="flex min-h-0 flex-1">
        <aside
          className={cn(
            "bg-chrome hidden shrink-0 flex-col transition-[width] duration-200 ease-out motion-reduce:transition-none md:flex",
            collapsed ? "w-[60px]" : "w-[220px]"
          )}
        >
          <nav
            aria-label="Sections"
            className={cn(
              "flex flex-1 scrollbar-thin flex-col gap-4 overflow-x-hidden overflow-y-auto pt-2 pb-5",
              collapsed ? "px-2" : "pr-4 pl-4"
            )}
          >
            <SidebarSections collapsed={collapsed} workspace={workspace} />
          </nav>
          <div className="bg-chrome shrink-0">
            <div
              className={cn(
                "border-line border-t py-2",
                collapsed ? "px-2" : "px-4"
              )}
            >
              {workspace ? (
                <WorkspaceSettings
                  collapsed={collapsed}
                  slug={workspace.slug}
                />
              ) : null}
              <NavRow
                collapsed={collapsed}
                icon={SidebarLeftIcon}
                label={collapsed ? "Expand menu" : "Collapse menu"}
                render={({ children: content, className }) => (
                  <button
                    aria-label={collapsed ? "Expand menu" : "Collapse menu"}
                    className={className}
                    onClick={toggle}
                    type="button"
                  >
                    {content}
                  </button>
                )}
              />
            </div>
          </div>
        </aside>

        <main className="bg-surface page-glow light:border-line relative mr-1 mb-1 ml-px min-w-0 flex-1 [scrollbar-gutter:stable] overflow-y-auto rounded-lg border border-transparent max-md:mx-1">
          {workspace?.mode === "test" ? <TestModeBar /> : null}
          {children}
        </main>
      </div>

      <Drawer.Root onOpenChange={setMenuOpen} open={menuOpen}>
        <Drawer.Portal>
          <Drawer.Backdrop className="bg-overlay fixed inset-0 z-50 transition-opacity duration-200 data-ending-style:opacity-0 data-starting-style:opacity-0 motion-reduce:transition-none" />
          <Drawer.Viewport className="fixed inset-0 z-50 flex">
            <Drawer.Popup className="bg-chrome border-line flex h-full w-[260px] flex-col overflow-y-auto border-r px-4 pt-4 pb-6 transition-transform duration-200 ease-out outline-none data-ending-style:-translate-x-full data-starting-style:-translate-x-full motion-reduce:transition-none">
              <Drawer.Title className="sr-only">Navigation</Drawer.Title>
              <SidebarSections
                onNavigate={() => setMenuOpen(false)}
                workspace={workspace}
              />
              {workspace ? (
                <div className="border-line mt-auto border-t pt-2">
                  <WorkspaceSettings
                    onNavigate={() => setMenuOpen(false)}
                    slug={workspace.slug}
                  />
                </div>
              ) : null}
            </Drawer.Popup>
          </Drawer.Viewport>
        </Drawer.Portal>
      </Drawer.Root>
    </div>
  );
};
