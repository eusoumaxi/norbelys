import { UserIcon } from "@hugeicons/core-free-icons";
import type { PersonObject, SegmentObject } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";

import { Copyable } from "@/components/copy";
import { Dash, listedItem, ListTable } from "@/components/data-table";
import { DetailSection, DetailsAside } from "@/components/details";
import {
  EditDeleteActions,
  PageBody,
  PageHeader,
  Section,
} from "@/components/page";
import { Problem } from "@/components/problem";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { PERSON_COLUMNS } from "@/features/people/columns";
import { fieldsQuery, peopleListQuery } from "@/features/people/queries";
import { DeleteSegmentDialog } from "@/features/segments/delete-segment";
import { subjectsOf } from "@/features/segments/filter";
import { FilterWords } from "@/features/segments/filter-words";
import { segmentQuery, segmentsKey } from "@/features/segments/queries";
import { SegmentDialog } from "@/features/segments/segment-dialog";
import { formatCount, formatTimestamp, plural } from "@/lib/format";
import { canWrite, useWorkspace } from "@/lib/workspace";

/**
 * The people a segment matches, in words: "1,234 people", "10,000+ people" past the count's cap,
 * or "Counting…" until a count exists (a listed segment carries none).
 */
const countText = (segment: SegmentObject): string => {
  if (segment.computed_at === null || segment.computed_at === undefined) {
    return "Counting…";
  }
  const count = segment.people_count ?? 0;
  return segment.people_count_capped
    ? `${formatCount(count)}+ people`
    : plural(count, "person", "people");
};

/** Edit and Delete, for people who may change the audience. */
const SegmentActions = ({ segment }: { segment: SegmentObject }) => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  return (
    <EditDeleteActions
      renderDelete={(dialog) => (
        <DeleteSegmentDialog
          {...dialog}
          onDeleted={() =>
            navigate({
              params: { slug: workspace.slug },
              to: "/w/$slug/segments",
            })
          }
          segment={segment}
        />
      )}
      renderEdit={(dialog) => <SegmentDialog {...dialog} segment={segment} />}
    />
  );
};

/** The people the filter matches now, newest first; a row opens the person. */
const SegmentPeople = ({ segment }: { segment: SegmentObject }) => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  return (
    <Section title={countText(segment)}>
      <ListTable<PersonObject>
        columns={[
          PERSON_COLUMNS.email,
          PERSON_COLUMNS.name,
          PERSON_COLUMNS.company,
          PERSON_COLUMNS.added,
        ]}
        empty={{
          description:
            "Nobody matches this filter now. People who come to match it join it at once.",
          icon: UserIcon,
          title: "No people match",
        }}
        onRowClick={(p) => {
          void navigate({
            params: { personId: p.id, slug: workspace.slug },
            to: "/w/$slug/people/$personId",
          });
        }}
        query={peopleListQuery(workspace, { segmentId: segment.id })}
        rowKey={(p) => p.id}
      />
    </Section>
  );
};

/** The page while the segment is read for the first time. */
const SegmentSkeleton = () => (
  <PageBody>
    <div className="mb-5 flex flex-col gap-3">
      <Skeleton className="h-8 w-64" />
      <Skeleton className="h-4 w-80" />
    </div>
    <Skeleton className="h-32 w-full" />
  </PageBody>
);

/**
 * One segment: its filter in words, the people it matches now (`people.list` with `segment_id`)
 * and its count, read by `segments.retrieve` (which counts up to 10,000 each time; "Count again"
 * reads it anew). A segment opened from the list shows at once, its count "Counting…" until the
 * read answers.
 */
const SegmentPage = () => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const { segmentId } = Route.useParams();
  const editable = canWrite(workspace);
  const read = useQuery({
    ...segmentQuery(workspace, segmentId),
    placeholderData: () =>
      listedItem<SegmentObject>(
        queryClient,
        [...segmentsKey(workspace), "list"],
        segmentId
      )?.item,
  });
  const fields = useQuery(fieldsQuery(workspace));
  const segment = read.data;
  if (!segment) {
    return read.isError ? (
      <PageBody>
        <Problem
          error={read.error}
          onRetry={() => {
            void read.refetch();
          }}
        />
      </PageBody>
    ) : (
      <SegmentSkeleton />
    );
  }
  const counted = segment.computed_at;
  return (
    <PageBody>
      <PageHeader
        actions={editable ? <SegmentActions segment={segment} /> : null}
        compact
        back={{
          label: "Segments",
          link: {
            params: { slug: workspace.slug },
            to: "/w/$slug/segments",
          },
        }}
        subtitle={countText(segment)}
        title={segment.name}
      />
      <div className="flex flex-col gap-8 lg:flex-row">
        <div className="flex min-w-0 flex-1 flex-col gap-8">
          <Card>
            <CardHeader>
              <CardTitle>Filter</CardTitle>
              <span className="text-fg-3 text-xs">
                {segment.filter.match === "any"
                  ? "Any condition matches"
                  : "Every condition must match"}
              </span>
            </CardHeader>
            <CardContent>
              {fields.isPending ? (
                <Skeleton className="h-5 w-72" />
              ) : (
                <FilterWords
                  filter={segment.filter}
                  subjects={subjectsOf(fields.data ?? [])}
                />
              )}
            </CardContent>
          </Card>
          <SegmentPeople segment={segment} />
        </div>
        <DetailsAside>
          <DetailSection
            rows={[
              { label: "ID", value: <Copyable mono value={segment.id} /> },
              { label: "Created", value: formatTimestamp(segment.created_at) },
              { label: "Updated", value: formatTimestamp(segment.updated_at) },
            ]}
            title="Segment"
          />
          <DetailSection
            action={
              <button
                className="text-link hover:text-link-hover disabled:text-fg-4 cursor-pointer font-semibold disabled:cursor-not-allowed"
                disabled={read.isFetching}
                onClick={() => {
                  void read.refetch();
                }}
                type="button"
              >
                {read.isFetching ? "Counting…" : "Count again"}
              </button>
            }
            rows={[
              { label: "People", value: countText(segment) },
              {
                label: "Counted",
                value: counted ? formatTimestamp(counted) : <Dash />,
              },
            ]}
            title="Count"
          />
        </DetailsAside>
      </div>
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/segments/$segmentId")({
  head: () => ({ meta: [{ title: "Segment · Norbelys" }] }),
  component: SegmentPage,
});
