import { Add01Icon, UserGroupIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { GroupObject } from "@norbelys/sdk";
import { createFileRoute } from "@tanstack/react-router";
import {
  createStandardSchemaV1,
  debounce,
  parseAsString,
  useQueryState,
} from "nuqs";
import { useDeferredValue } from "react";

import { Dash, ListTable, NameCell } from "@/components/data-table";
import { PageBody, PageHeader } from "@/components/page";
import { SearchInput } from "@/components/search-input";
import { Button } from "@/components/ui/button";
import { GroupActions } from "@/features/groups/group-actions";
import { GroupDialog, NEW_GROUP } from "@/features/groups/group-dialog";
import { groupListQuery } from "@/features/groups/queries";
import { useUrlDialog } from "@/hooks/use-url-dialog";
import { formatRelative, plural } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

// Declared once for nuqs (state) and the router (typed links, loader dependencies).
const search = {
  group: parseAsString,
  q: parseAsString.withDefault(""),
};

const CreateGroupButton = () => {
  const dialog = useUrlDialog("group");
  return (
    <Button onClick={() => dialog.open(NEW_GROUP)} variant="primary">
      <HugeiconsIcon icon={Add01Icon} />
      Create group
    </Button>
  );
};

/** Lists of people chosen by hand, used as campaign audiences. */
const GroupsPage = () => {
  const workspace = useWorkspace();
  const dialog = useUrlDialog("group");
  // The input follows every keystroke; the URL, and so the list, follows 300 ms later.
  const [input, setInput] = useQueryState(
    "q",
    search.q.withOptions({ limitUrlUpdates: debounce(300) })
  );
  const q = useDeferredValue(input);
  return (
    <PageBody>
      <PageHeader actions={<CreateGroupButton />} title="Groups" />
      <div className="flex flex-col gap-2">
        <SearchInput
          label="Search groups"
          maxLength={256}
          onChange={(value) => {
            void setInput(value || null);
          }}
          value={input}
        />
        <ListTable<GroupObject>
          columns={[
            {
              render: (g) => <NameCell icon={UserGroupIcon}>{g.name}</NameCell>,
              header: "Name",
              id: "name",
            },
            {
              render: (g) =>
                g.description ? (
                  <span className="text-fg-2 block max-w-[360px] truncate">
                    {g.description}
                  </span>
                ) : (
                  <Dash />
                ),
              header: "Description",
              id: "description",
            },
            {
              render: (g) => plural(g.people_count, "person", "people"),
              header: "People",
              id: "people",
            },
            {
              render: (g) => formatRelative(g.updated_at),
              header: "Updated",
              id: "updated",
            },
            {
              render: (g) => (
                <GroupActions group={g} onEdit={() => dialog.open(g.id)} />
              ),
              className: "w-[62px]",
              header: "",
              id: "menu",
            },
          ]}
          empty={{
            action: q ? undefined : <CreateGroupButton />,
            description: q
              ? "No group matches this search."
              : "A group is a list of people you choose, to use as a campaign audience.",
            icon: UserGroupIcon,
            title: q ? "No results" : "No groups yet",
          }}
          onRowClick={(g) => dialog.open(g.id)}
          query={groupListQuery(workspace, q)}
          rowKey={(g) => g.id}
        />
      </div>
      <GroupDialog />
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/groups")({
  validateSearch: createStandardSchemaV1(search, { partialOutput: true }),
  head: () => ({ meta: [{ title: "Groups · Norbelys" }] }),
  component: GroupsPage,
});
