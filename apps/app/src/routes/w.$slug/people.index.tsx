import { Add01Icon, UserIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { PersonObject } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import {
  createStandardSchemaV1,
  debounce,
  parseAsString,
  useQueryState,
} from "nuqs";
import { useDeferredValue, useState } from "react";
import { toast } from "sonner";

import { CreateDialog } from "@/components/create-dialog";
import { Dash, ListTable } from "@/components/data-table";
import { PageBody, PageHeader } from "@/components/page";
import { CopyIdItem, RowMenu } from "@/components/row-menu";
import { SearchInput } from "@/components/search-input";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { Select } from "@/components/ui/select";
import { ImportButton } from "@/features/imports/import-button";
import { PERSON_COLUMNS } from "@/features/people/columns";
import { DeletePersonDialog } from "@/features/people/delete-person";
import {
  fieldsQuery,
  groupOptionsQuery,
  peopleKey,
  peopleListQuery,
} from "@/features/people/queries";
import {
  CustomFieldMatches,
  SearchMatch,
} from "@/features/people/search-matches";
import { formatName, formatRelative } from "@/lib/format";
import { canWrite, useWorkspace } from "@/lib/workspace";

// Declared once for nuqs (state) and the router (typed links, such as a group's "View people").
const search = {
  group: parseAsString,
  q: parseAsString.withDefault(""),
};

/** Whether a person replied, or when they were last sent to. */
const Activity = ({ person }: { person: PersonObject }) => {
  if (person.replied_at) {
    return (
      <Badge dot tone="success">
        Replied
      </Badge>
    );
  }
  if (person.last_sent_at) {
    return <>Sent {formatRelative(person.last_sent_at).toLowerCase()}</>;
  }
  return <Dash />;
};

/** A person's groups by name: the first, then how many more. */
const GroupNames = ({
  groupIds,
  names,
}: {
  groupIds: string[];
  names: Map<string, string>;
}) => {
  const [first] = groupIds;
  if (!first) {
    return <Dash />;
  }
  return (
    <span className="flex items-center gap-1">
      <Badge className="max-w-40">
        <span className="truncate">{names.get(first) ?? "1 group"}</span>
      </Badge>
      {groupIds.length > 1 ? (
        <span className="text-fg-3 text-xs">+{groupIds.length - 1}</span>
      ) : null}
    </span>
  );
};

/**
 * Everyone in the workspace, newest first: search all contact details and custom values,
 * and a group filter (`people.list` with `q` and `group_id`, both kept in the address). A row
 * opens the person; writers can add one, import a file or delete from the row's menu.
 */
const PeoplePage = () => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const editable = canWrite(workspace);
  const [creating, setCreating] = useState(false);
  const [deleting, setDeleting] = useState<PersonObject | null>(null);
  // The input follows every keystroke; the URL, and so the list, follows 300 ms later.
  const [input, setInput] = useQueryState(
    "q",
    search.q.withOptions({ limitUrlUpdates: debounce(300) })
  );
  const [groupId, setGroupId] = useQueryState("group", search.group);
  const q = useDeferredValue(input.trim());
  const groups = useQuery(groupOptionsQuery(workspace));
  const fields = useQuery(fieldsQuery(workspace));
  const names = new Map(
    (groups.data ?? []).map((group) => [group.id, group.name])
  );
  const filtered = Boolean(q || groupId);
  const clearFilters = () => {
    void setInput(null);
    void setGroupId(null);
  };
  let emptyDescription =
    "Add people one by one, import a CSV file, or create them through the API.";
  if (q) {
    emptyDescription = `No matches for “${q}”${groupId ? " in this group" : ""}. Try a shorter search or clear the filters.`;
  } else if (groupId) {
    emptyDescription =
      "This group has no people yet. Clear the filters to see everyone.";
  }
  const open = (person: PersonObject) => {
    void navigate({
      params: { personId: person.id, slug: workspace.slug },
      to: "/w/$slug/people/$personId",
    });
  };
  const actions = editable ? (
    <>
      <ImportButton variant="secondary" />
      <Button onClick={() => setCreating(true)} variant="primary">
        <HugeiconsIcon icon={Add01Icon} />
        Add person
      </Button>
    </>
  ) : null;

  return (
    <PageBody>
      <PageHeader
        actions={actions}
        subtitle="Find and manage everyone in your audience."
        title="People"
      />
      <div className="flex flex-col gap-3">
        <div className="flex flex-col gap-2 sm:flex-row">
          <SearchInput
            describedBy="people-search-help"
            label="Search people"
            maxLength={256}
            onChange={(value) => {
              void setInput(value || null);
            }}
            placeholder="Search email, name, company, phone or custom fields…"
            value={input}
          />
          <Select
            className="sm:w-60"
            label="Group"
            onChange={(value) => {
              void setGroupId(value || null);
            }}
            options={[
              { label: "All groups", value: "" },
              ...(groups.data ?? []).map((group) => ({
                label: group.name,
                value: group.id,
              })),
            ]}
            value={groupId ?? ""}
          />
        </div>
        <div className="flex flex-wrap items-center justify-between gap-2">
          <p className="text-fg-3 text-xs" id="people-search-help">
            Search any part of a contact’s details, including all custom field
            values.
          </p>
          {filtered ? (
            <Button onClick={clearFilters} size="s" variant="tertiary">
              Clear filters
            </Button>
          ) : null}
        </div>
        <ListTable<PersonObject>
          columns={[
            {
              ...PERSON_COLUMNS.email,
              render: (p) => (
                <span className="text-fg font-bold">
                  <SearchMatch query={q} text={p.email} />
                </span>
              ),
            },
            {
              ...PERSON_COLUMNS.name,
              render: (p) => {
                const name = formatName(p);
                return name ? <SearchMatch query={q} text={name} /> : <Dash />;
              },
            },
            {
              ...PERSON_COLUMNS.company,
              render: (p) =>
                p.company ? (
                  <SearchMatch query={q} text={p.company} />
                ) : (
                  <Dash />
                ),
            },
            ...(q
              ? [
                  {
                    render: (p: PersonObject) => (
                      <CustomFieldMatches
                        definitions={fields.data ?? []}
                        person={p}
                        query={q}
                      />
                    ),
                    header: "Matching fields",
                    id: "matches",
                  },
                ]
              : []),
            {
              render: (p) => (
                <GroupNames groupIds={p.group_ids} names={names} />
              ),
              header: "Groups",
              id: "groups",
            },
            {
              render: (p) => <Activity person={p} />,
              header: "Activity",
              id: "activity",
            },
            PERSON_COLUMNS.added,
            {
              render: (p) => (
                <RowMenu label={`Actions for ${p.email}`}>
                  <DropdownMenuItem onClick={() => open(p)}>
                    Open
                  </DropdownMenuItem>
                  <CopyIdItem id={p.id} noun="person" />
                  {editable ? (
                    <DropdownMenuItem
                      className="text-error-fg"
                      onClick={() => setDeleting(p)}
                    >
                      Delete
                    </DropdownMenuItem>
                  ) : null}
                </RowMenu>
              ),
              className: "w-[62px]",
              header: "",
              id: "menu",
            },
          ]}
          empty={{
            action: filtered ? (
              <Button onClick={clearFilters} variant="secondary">
                Clear filters
              </Button>
            ) : (
              actions
            ),
            description: emptyDescription,
            icon: UserIcon,
            illustration: filtered ? undefined : "people",
            title: filtered ? "No people found" : "No people yet",
          }}
          onRowClick={open}
          query={peopleListQuery(workspace, { groupId, q })}
          rowKey={(p) => p.id}
        />
      </div>
      <CreateDialog
        fields={[
          {
            label: "Email",
            name: "email",
            placeholder: "ada@example.com",
            required: true,
            type: "email",
          },
          { label: "First name", name: "given_name", placeholder: "Ada" },
          { label: "Last name", name: "family_name", placeholder: "Lovelace" },
          {
            label: "Company",
            name: "company",
            placeholder: "Analytical Engines",
          },
        ]}
        onOpenChange={setCreating}
        onSubmit={async (values) => {
          const person = await workspace.api.people.create({
            company: values.company,
            email: values.email ?? "",
            family_name: values.family_name,
            given_name: values.given_name,
          });
          toast.success("Person added");
          void queryClient.invalidateQueries({
            queryKey: peopleKey(workspace),
          });
          setCreating(false);
          open(person);
        }}
        open={creating}
        submitLabel="Add person"
        title="Add person"
      />
      <DeletePersonDialog
        onOpenChange={(next) => {
          if (!next) {
            setDeleting(null);
          }
        }}
        open={deleting !== null}
        person={deleting}
      />
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/people/")({
  validateSearch: createStandardSchemaV1(search, { partialOutput: true }),
  head: () => ({ meta: [{ title: "People · Norbelys" }] }),
  component: PeoplePage,
});
