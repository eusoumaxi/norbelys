import { Dialog as DialogPrimitive } from "@base-ui/react/dialog";
import {
  Building03Icon,
  Megaphone01Icon,
  Search01Icon,
  UserIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { IconSvgElement } from "@hugeicons/react";
import { skipToken, useQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import type { NavigateOptions } from "@tanstack/react-router";
import { cn } from "cn";
import { useDeferredValue, useEffect, useMemo, useState } from "react";

import { workspaceLinks, workspaceRoute } from "@/components/shell/nav";
import { campaignOptionsQuery } from "@/features/campaigns/queries";
import { useSession } from "@/lib/auth";
import { formatName } from "@/lib/format";
import { statusLabel } from "@/lib/status";
import type { Workspace } from "@/lib/workspace";

interface Command {
  id: string;
  group: string;
  label: string;
  hint?: string;
  icon: IconSvgElement;
  to: NavigateOptions;
}

const isMac = () =>
  typeof navigator !== "undefined" &&
  /Mac|iPhone|iPad/u.test(navigator.platform);

/** The people whose name or address matches what was typed (two characters or more). */
const usePeopleMatches = (
  workspace: Workspace | undefined,
  needle: string,
  open: boolean
) =>
  useQuery({
    enabled: Boolean(workspace) && open && needle.length >= 2,
    queryFn: async ({ signal }) =>
      workspace
        ? await workspace.api.people.list({ limit: 5, q: needle }, { signal })
        : null,
    queryKey: [workspace?.id, "people", "command", needle],
    staleTime: 30_000,
  });

/**
 * The top bar's search: a 250px field that opens a palette (also on ⌘K or Ctrl+K) to go anywhere
 * by name: the workspace's campaigns and pages, people by name or address, the person's
 * workspaces and the account.
 */
export const CommandMenu = ({ workspace }: { workspace?: Workspace }) => {
  const session = useSession();
  const navigate = useNavigate();
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setOpen((value) => !value);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const options = workspace ? campaignOptionsQuery(workspace) : null;
  const campaigns = useQuery({
    queryFn: options?.queryFn && open ? options.queryFn : skipToken,
    queryKey: options?.queryKey ?? ["campaigns", "none"],
  });
  const needle = useDeferredValue(query.trim().toLowerCase());
  const people = usePeopleMatches(workspace, needle, open);

  const commands = useMemo<Command[]>(() => {
    const pages: Command[] = workspace
      ? workspaceLinks.map((link) => ({
          group: workspace.name,
          icon: link.icon,
          id: `page:${link.path}`,
          label: link.label,
          to: {
            params: { slug: workspace.slug },
            to: workspaceRoute(link.path),
          },
        }))
      : [];
    const workspaces: Command[] = session.memberships.map((membership) => ({
      group: "Workspaces",
      hint: membership.workspace.slug,
      icon: Building03Icon,
      id: `ws:${membership.workspace.id}`,
      label: membership.workspace.name,
      to: { params: { slug: membership.workspace.slug }, to: "/w/$slug" },
    }));
    const account: Command[] = [
      {
        group: "Account",
        icon: UserIcon,
        id: "account",
        label: "Account settings",
        to: { to: "/account" },
      },
    ];
    const campaignCommands: Command[] = workspace
      ? (campaigns.data?.data ?? []).map((campaign) => ({
          group: "Campaigns",
          hint: statusLabel("campaign", campaign.status),
          icon: Megaphone01Icon,
          id: `campaign:${campaign.id}`,
          label: campaign.name,
          to: {
            params: { campaignId: campaign.id, slug: workspace.slug },
            to: "/w/$slug/campaigns/$campaignId",
          },
        }))
      : [];
    return [...pages, ...campaignCommands, ...workspaces, ...account];
  }, [campaigns.data, session.memberships, workspace]);

  // Campaigns wait for a word, so the open palette stays a short list of pages.
  const matches = needle
    ? commands.filter(
        (command) =>
          command.label.toLowerCase().includes(needle) ||
          command.hint?.toLowerCase().includes(needle)
      )
    : commands.filter((command) => command.group !== "Campaigns");
  const personCommands: Command[] =
    workspace && needle.length >= 2
      ? (people.data?.data ?? []).map((person) => ({
          group: "People",
          hint: formatName(person) ? person.email : undefined,
          icon: UserIcon,
          id: `person:${person.id}`,
          label: formatName(person) || person.email,
          to: {
            params: { personId: person.id, slug: workspace.slug },
            to: "/w/$slug/people/$personId",
          },
        }))
      : [];
  const results = [...matches, ...personCommands];

  const run = (command: Command | undefined) => {
    if (!command) {
      return;
    }
    setOpen(false);
    void navigate(command.to);
  };

  let lastGroup = "";
  return (
    <DialogPrimitive.Root
      onOpenChange={(value) => {
        setOpen(value);
        if (!value) {
          setQuery("");
          setActive(0);
        }
      }}
      open={open}
    >
      <DialogPrimitive.Trigger className="border-line-strong bg-surface text-fg-3 hover:border-focus focus-visible:outline-focus hidden h-7 w-[250px] cursor-pointer items-center gap-3 rounded-sm border pr-4 pl-3 text-left text-base transition-colors outline-none focus-visible:outline-1 md:flex">
        <HugeiconsIcon
          className="text-icon size-3.5 shrink-0"
          icon={Search01Icon}
        />
        <span className="flex-1 truncate">Search...</span>
        <kbd className="text-fg-3 font-sans text-xs">
          {isMac() ? "⌘K" : "Ctrl K"}
        </kbd>
      </DialogPrimitive.Trigger>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Backdrop className="bg-overlay fixed inset-0 z-50 transition-opacity duration-150 data-ending-style:opacity-0 data-starting-style:opacity-0" />
        <DialogPrimitive.Popup className="border-line bg-surface shadow-menu fixed top-[12vh] left-1/2 z-50 flex max-h-[70vh] w-[calc(100%-32px)] max-w-[560px] -translate-x-1/2 flex-col overflow-hidden rounded-sm border outline-none">
          <DialogPrimitive.Title className="sr-only">
            Search
          </DialogPrimitive.Title>
          <div className="border-line flex h-12 items-center gap-3 border-b px-4">
            <HugeiconsIcon
              className="text-icon size-4 shrink-0"
              icon={Search01Icon}
            />
            <input
              aria-label="Search pages and workspaces"
              autoFocus
              className="text-fg placeholder:text-fg-3 h-full flex-1 bg-transparent text-base outline-none focus-visible:outline-none"
              onChange={(event) => {
                setQuery(event.target.value);
                setActive(0);
              }}
              onKeyDown={(event) => {
                if (event.key === "ArrowDown") {
                  event.preventDefault();
                  setActive((value) => Math.min(value + 1, results.length - 1));
                } else if (event.key === "ArrowUp") {
                  event.preventDefault();
                  setActive((value) => Math.max(value - 1, 0));
                } else if (event.key === "Enter") {
                  event.preventDefault();
                  run(results[active]);
                }
              }}
              placeholder="Find a page, a campaign or a person..."
              value={query}
            />
          </div>
          <div className="scrollbar-thin overflow-y-auto py-1">
            {results.length === 0 ? (
              <p className="text-fg-3 px-4 py-6 text-center text-sm">
                Nothing matches “{query}”.
              </p>
            ) : (
              results.map((command, index) => {
                const header =
                  command.group === lastGroup ? null : command.group;
                lastGroup = command.group;
                return (
                  <div key={command.id}>
                    {header ? (
                      <div className="text-fg-2 px-4 pt-2 pb-1 text-[10px] leading-[10px] font-semibold tracking-[0.5px] uppercase">
                        {header}
                      </div>
                    ) : null}
                    <button
                      data-active={index === active ? "" : undefined}
                      className={cn(
                        "text-fg flex h-8 w-full cursor-pointer items-center gap-3 px-4 text-left text-sm font-semibold outline-none",
                        index === active ? "bg-hover" : null
                      )}
                      onClick={() => run(command)}
                      onMouseMove={() => setActive(index)}
                      type="button"
                    >
                      <HugeiconsIcon
                        className="text-icon size-4 shrink-0"
                        icon={command.icon}
                      />
                      <span className="flex-1 truncate">{command.label}</span>
                      {command.hint ? (
                        <span className="text-fg-3 font-mono text-xs font-normal">
                          {command.hint}
                        </span>
                      ) : null}
                    </button>
                  </div>
                );
              })
            )}
          </div>
        </DialogPrimitive.Popup>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
};
