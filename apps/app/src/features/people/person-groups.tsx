import {
  Add01Icon,
  Cancel01Icon,
  UserGroupIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { GroupObject, PersonObject } from "@norbelys/sdk";
import { useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { groupQuery, groupsKey } from "@/features/groups/queries";
import {
  groupOptionsQuery,
  peopleKey,
  personKey,
} from "@/features/people/queries";
import { useAction } from "@/lib/actions";
import { shortId } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/** A person belongs to at most this many groups. */
const GROUPS_MAX = 100;

/**
 * The names of a person's groups: from the first 100 groups when they hold them, otherwise read
 * one by one (a workspace with more groups than one page).
 */
const useGroupNames = (groupIds: string[]) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const options = useQuery(groupOptionsQuery(workspace));
  const known = new Map<string, GroupObject>(
    (options.data ?? []).map((group) => [group.id, group])
  );
  const missing = options.data ? groupIds.filter((id) => !known.has(id)) : [];
  const extra = useQueries({
    queries: missing.map((id) => groupQuery(queryClient, workspace, id)),
  });
  for (const result of extra) {
    if (result.data) {
      known.set(result.data.id, result.data);
    }
  }
  return { known, options };
};

/** The "Add to group" menu: every listed group the person is not in yet. */
const AddToGroup = ({
  busy,
  onAdd,
  options,
  person,
}: {
  busy: boolean;
  onAdd: (group: GroupObject) => void;
  options: GroupObject[];
  person: PersonObject;
}) => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const available = options.filter(
    (group) => !person.group_ids.includes(group.id)
  );
  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        disabled={busy || person.group_ids.length >= GROUPS_MAX}
        render={<Button size="s" variant="secondary" />}
      >
        {busy ? <Spinner /> : <HugeiconsIcon icon={Add01Icon} />}
        Add to group
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="max-h-80">
        <DropdownMenuGroup>
          {available.length === 0 ? (
            <DropdownMenuItem disabled>No other groups</DropdownMenuItem>
          ) : null}
          {available.map((group) => (
            <DropdownMenuItem key={group.id} onClick={() => onAdd(group)}>
              <HugeiconsIcon icon={UserGroupIcon} />
              <span className="truncate">{group.name}</span>
            </DropdownMenuItem>
          ))}
        </DropdownMenuGroup>
        <DropdownMenuGroup>
          <DropdownMenuItem
            onClick={() => {
              void navigate({
                params: { slug: workspace.slug },
                search: { group: "new" },
                to: "/w/$slug/groups",
              });
            }}
          >
            <HugeiconsIcon icon={Add01Icon} />
            Create a group
          </DropdownMenuItem>
        </DropdownMenuGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  );
};

/**
 * The groups a person is in, as badges a writer can remove, and "Add to group". Both send the
 * whole new list as `people.update({group_ids})` (the API replaces the memberships whole) with
 * the person's version in `If-Match`, so two changes made at once never undo each other: the
 * second is refused and says so.
 */
export const PersonGroups = ({
  editable,
  person,
}: {
  editable: boolean;
  person: PersonObject;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const action = useAction();
  const { known, options } = useGroupNames(person.group_ids);
  const [busy, setBusy] = useState(false);

  const change = async (groupIds: string[], done: string) => {
    setBusy(true);
    await action(
      done,
      async () => {
        const saved = await workspace.api.people.update(
          person.id,
          { group_ids: groupIds },
          { headers: { "If-Match": `"${person.version}"` } }
        );
        queryClient.setQueryData(personKey(workspace, saved.id), saved);
      },
      () => {
        void queryClient.invalidateQueries({ queryKey: groupsKey(workspace) });
        void queryClient.invalidateQueries({
          queryKey: [...peopleKey(workspace), "list"],
        });
      }
    );
    setBusy(false);
  };

  let body = <p className="text-fg-3 text-sm">Not in any group.</p>;
  if (person.group_ids.length > 0) {
    body = (
      <ul className="flex flex-wrap gap-1.5">
        {person.group_ids.map((id) => {
          const name = known.get(id)?.name ?? shortId(id);
          return (
            <li key={id}>
              <Badge className="h-6 gap-1 pr-1 text-xs">
                <Link
                  className="hover:text-link max-w-60 truncate"
                  params={{ slug: workspace.slug }}
                  search={{ group: id }}
                  to="/w/$slug/groups"
                >
                  {name}
                </Link>
                {editable ? (
                  <button
                    aria-label={`Remove from ${name}`}
                    className="text-fg-3 hover:text-fg focus-visible:outline-focus flex size-4 cursor-pointer items-center justify-center rounded-xs outline-none focus-visible:outline-1 disabled:cursor-not-allowed"
                    disabled={busy}
                    onClick={() => {
                      void change(
                        person.group_ids.filter((other) => other !== id),
                        `Removed from ${name}`
                      );
                    }}
                    type="button"
                  >
                    <HugeiconsIcon className="size-3" icon={Cancel01Icon} />
                  </button>
                ) : null}
              </Badge>
            </li>
          );
        })}
      </ul>
    );
  }

  return (
    <Card>
      <CardHeader>
        <div className="flex flex-col gap-0.5">
          <CardTitle>Groups</CardTitle>
          <CardDescription className="text-xs">
            Lists you chose this person for, usable as campaign audiences.
          </CardDescription>
        </div>
        {editable && options.data ? (
          <AddToGroup
            busy={busy}
            onAdd={(group) => {
              void change(
                [...person.group_ids, group.id],
                `Added to ${group.name}`
              );
            }}
            options={options.data}
            person={person}
          />
        ) : null}
      </CardHeader>
      <CardContent>
        {options.isPending && person.group_ids.length > 0 ? (
          <Skeleton className="h-6 w-48" />
        ) : (
          body
        )}
      </CardContent>
    </Card>
  );
};
