import {
  Add01Icon,
  Building03Icon,
  Settings02Icon,
  Tick02Icon,
  UnfoldMoreIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useNavigate } from "@tanstack/react-router";
import { cn } from "cn";

import { Slash } from "@/components/slash";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useSession } from "@/lib/auth";
import { humanize } from "@/lib/format";
import type { Workspace } from "@/lib/workspace";

/**
 * A workspace's square: its initial in grey, the way people tell several apart in a list. Grey,
 * not pink: next to the logo, a second pink mark would compete with it and mean nothing.
 */
export const WorkspaceMark = ({
  className,
  name,
}: {
  className?: string;
  name: string;
}) => (
  <span
    aria-hidden
    className={cn(
      "bg-hover text-fg-2 grid size-5 shrink-0 place-items-center rounded-xs text-xs font-semibold uppercase",
      className
    )}
  >
    {name.trim().charAt(0) || "·"}
  </span>
);

/**
 * The workspace, after the logo in the top bar: "Norbelys / Acme". Most people have one
 * workspace, so it reads first as where they are; its menu switches to another one (landing on
 * its overview), creates one, or opens the workspace's settings and the list of all of them.
 */
export const WorkspaceSwitcher = ({ workspace }: { workspace: Workspace }) => {
  const session = useSession();
  const navigate = useNavigate();
  const others = session.memberships.length > 1;
  return (
    <>
      <Slash />
      <DropdownMenu>
        <DropdownMenuTrigger
          aria-label={`${workspace.name}: workspace menu`}
          className="hover:bg-hover data-popup-open:bg-hover focus-visible:outline-focus flex h-8 min-w-0 cursor-pointer items-center gap-2 rounded-sm px-1.5 text-left transition-colors outline-none focus-visible:outline-1"
        >
          <WorkspaceMark name={workspace.name} />
          <span className="text-fg max-w-[200px] min-w-0 truncate text-sm font-semibold">
            {workspace.name}
          </span>
          <HugeiconsIcon
            className="text-fg-3 size-3.5 shrink-0"
            icon={UnfoldMoreIcon}
          />
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start" className="w-64">
          <DropdownMenuGroup>
            <DropdownMenuLabel>
              {others ? "Switch workspace" : "Workspace"}
            </DropdownMenuLabel>
            {session.memberships.map((membership) => (
              <DropdownMenuItem
                className="h-auto gap-2.5 py-1.5"
                key={membership.id}
                onClick={() => {
                  void navigate({
                    params: { slug: membership.workspace.slug },
                    to: "/w/$slug",
                  });
                }}
              >
                <WorkspaceMark
                  className="size-6"
                  name={membership.workspace.name}
                />
                <span className="flex min-w-0 flex-1 flex-col">
                  <span className="truncate">{membership.workspace.name}</span>
                  <span className="text-fg-3 truncate text-xs font-normal">
                    {humanize(membership.role)}
                    {membership.workspace.mode === "test" ? " · Test mode" : ""}
                  </span>
                </span>
                {membership.workspace.id === workspace.id ? (
                  <HugeiconsIcon className="text-accent!" icon={Tick02Icon} />
                ) : null}
              </DropdownMenuItem>
            ))}
          </DropdownMenuGroup>
          <DropdownMenuGroup>
            <DropdownMenuItem
              onClick={() => {
                void navigate({
                  params: { slug: workspace.slug },
                  to: "/w/$slug/settings",
                });
              }}
            >
              <HugeiconsIcon icon={Settings02Icon} />
              Workspace settings
            </DropdownMenuItem>
            <DropdownMenuItem
              onClick={() => {
                void navigate({ to: "/new" });
              }}
            >
              <HugeiconsIcon icon={Add01Icon} />
              New workspace
            </DropdownMenuItem>
            {others ? (
              <DropdownMenuItem
                onClick={() => {
                  void navigate({ to: "/workspaces" });
                }}
              >
                <HugeiconsIcon icon={Building03Icon} />
                All workspaces
              </DropdownMenuItem>
            ) : null}
          </DropdownMenuGroup>
        </DropdownMenuContent>
      </DropdownMenu>
    </>
  );
};
