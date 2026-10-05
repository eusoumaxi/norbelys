import type { PersonObject } from "@norbelys/sdk";
import { useSuspenseQuery } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";

import { Copyable } from "@/components/copy";
import { DetailSection, DetailsAside } from "@/components/details";
import { EditDeleteActions, PageBody, PageHeader } from "@/components/page";
import { DeletePersonDialog } from "@/features/people/delete-person";
import { PersonActivity } from "@/features/people/person-activity";
import { PersonDialog } from "@/features/people/person-dialog";
import { PersonFields } from "@/features/people/person-fields";
import { PersonGroups } from "@/features/people/person-groups";
import { personQuery } from "@/features/people/queries";
import { formatName, formatTimestamp } from "@/lib/format";
import { canWrite, useWorkspace } from "@/lib/workspace";

/** The line under the address: the person's name and company, or that they have neither. */
const Identity = ({ person }: { person: PersonObject }) => {
  const line = [formatName(person), person.company].filter(Boolean).join(" · ");
  return line ? (
    <span className="truncate">{line}</span>
  ) : (
    <span className="text-fg-3">No name or company</span>
  );
};

/** Edit and Delete, for people who may change the audience. */
const PersonActions = ({ person }: { person: PersonObject }) => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  return (
    <EditDeleteActions
      renderDelete={(dialog) => (
        <DeletePersonDialog
          {...dialog}
          onDeleted={() =>
            navigate({
              params: { slug: workspace.slug },
              to: "/w/$slug/people",
            })
          }
          person={person}
        />
      )}
      renderEdit={(dialog) => <PersonDialog {...dialog} person={person} />}
    />
  );
};

/**
 * One person: the address as the title with the name and company under it, their custom values
 * typed by each field's definition, their groups, their activity (messages, enrollments,
 * conversations) and the details aside. Writers edit and delete from the header.
 */
const PersonPage = () => {
  const workspace = useWorkspace();
  const { personId } = Route.useParams();
  const { data: person } = useSuspenseQuery(personQuery(workspace, personId));
  const editable = canWrite(workspace);
  return (
    <PageBody>
      <PageHeader
        actions={editable ? <PersonActions person={person} /> : null}
        compact
        back={{
          label: "People",
          link: {
            params: { slug: workspace.slug },
            to: "/w/$slug/people",
          },
        }}
        subtitle={<Identity person={person} />}
        title={person.email}
      />
      <div className="flex flex-col gap-8 lg:flex-row">
        <div className="flex min-w-0 flex-1 flex-col gap-6">
          <PersonFields editable={editable} person={person} />
          <PersonGroups editable={editable} person={person} />
          <PersonActivity personId={person.id} />
        </div>
        <DetailsAside>
          <DetailSection
            rows={[
              { label: "ID", value: <Copyable mono value={person.id} /> },
              { label: "Email", value: <Copyable value={person.email} /> },
              { label: "Created", value: formatTimestamp(person.created_at) },
              { label: "Updated", value: formatTimestamp(person.updated_at) },
            ]}
            title="Person"
          />
          <DetailSection
            rows={[
              {
                label: "Last sent",
                value: person.last_sent_at
                  ? formatTimestamp(person.last_sent_at)
                  : "Never",
              },
              {
                label: "Replied",
                value: person.replied_at
                  ? formatTimestamp(person.replied_at)
                  : "No reply yet",
              },
              {
                label: "Groups",
                value: String(person.group_ids.length),
              },
            ]}
            title="Engagement"
          />
        </DetailsAside>
      </div>
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/people/$personId")({
  loader: ({ context, params }) =>
    context.queryClient.ensureQueryData(
      personQuery(context.workspace, params.personId)
    ),
  head: ({ loaderData }) => ({
    meta: [{ title: `${loaderData?.email ?? "Person"} · Norbelys` }],
  }),
  component: PersonPage,
});
