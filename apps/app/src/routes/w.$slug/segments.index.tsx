import { Add01Icon, FilterIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { SegmentObject } from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";

import { ListTable, NameCell } from "@/components/data-table";
import { PageBody, PageHeader } from "@/components/page";
import { CopyIdItem, RowMenu } from "@/components/row-menu";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { Skeleton } from "@/components/ui/skeleton";
import { fieldsQuery } from "@/features/people/queries";
import { DeleteSegmentDialog } from "@/features/segments/delete-segment";
import { subjectsOf } from "@/features/segments/filter";
import { FilterSummary } from "@/features/segments/filter-words";
import { segmentListQuery } from "@/features/segments/queries";
import { SegmentDialog } from "@/features/segments/segment-dialog";
import { formatRelative } from "@/lib/format";
import { DOCS } from "@/lib/links";
import { canWrite, useWorkspace } from "@/lib/workspace";

/** What the segments page is doing besides listing. */
type Mode = "create" | "edit" | "delete" | null;

/**
 * Saved filters over people, newest first, each with its filter in words; a row opens the
 * segment, where its people are counted (a list leaves the counts out, because counting reads
 * every person once). Writers create, edit and delete them here.
 */
const SegmentsPage = () => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const editable = canWrite(workspace);
  const fields = useQuery(fieldsQuery(workspace));
  const subjects = subjectsOf(fields.data ?? []);
  const [mode, setMode] = useState<Mode>(null);
  // The segment being edited or deleted; it stays while its dialog closes.
  const [target, setTarget] = useState<SegmentObject | null>(null);
  const start = (next: Mode, segment: SegmentObject | null) => {
    setTarget(segment);
    setMode(next);
  };
  const open = (segment: SegmentObject) => {
    void navigate({
      params: { segmentId: segment.id, slug: workspace.slug },
      to: "/w/$slug/segments/$segmentId",
    });
  };
  const close = (next: boolean) => {
    if (!next) {
      setMode(null);
    }
  };
  const createButton = editable ? (
    <Button onClick={() => start("create", null)} variant="primary">
      <HugeiconsIcon icon={Add01Icon} />
      Create segment
    </Button>
  ) : null;

  return (
    <PageBody>
      <PageHeader actions={createButton} title="Segments" />
      <ListTable<SegmentObject>
        columns={[
          {
            render: (s) => <NameCell icon={FilterIcon}>{s.name}</NameCell>,
            header: "Name",
            id: "name",
          },
          {
            render: (s) =>
              fields.isPending ? (
                <Skeleton className="h-3.5 w-48" />
              ) : (
                <FilterSummary filter={s.filter} subjects={subjects} />
              ),
            className: "max-w-[480px]",
            header: "Filter",
            id: "filter",
          },
          {
            render: (s) => formatRelative(s.updated_at),
            header: "Updated",
            id: "updated",
          },
          {
            render: (s) => (
              <RowMenu label={`Actions for ${s.name}`}>
                <DropdownMenuItem onClick={() => open(s)}>
                  Open and count
                </DropdownMenuItem>
                {editable ? (
                  <DropdownMenuItem onClick={() => start("edit", s)}>
                    Edit segment
                  </DropdownMenuItem>
                ) : null}
                <CopyIdItem id={s.id} noun="segment" />
                {editable ? (
                  <DropdownMenuItem
                    className="text-error-fg"
                    onClick={() => start("delete", s)}
                  >
                    Delete segment
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
          action: createButton ?? undefined,
          description: (
            <>
              A segment is a saved filter over people and their custom fields;
              whoever matches it when it is read is in it.{" "}
              <a
                href={`${DOCS}/people-and-audiences`}
                rel="noreferrer"
                target="_blank"
              >
                Read about segments
              </a>
            </>
          ),
          icon: FilterIcon,
          title: "No segments yet",
        }}
        onRowClick={open}
        query={segmentListQuery(workspace)}
        rowKey={(s) => s.id}
      />
      <SegmentDialog
        onOpenChange={close}
        onSaved={(saved) => {
          if (mode === "create") {
            open(saved);
          }
        }}
        open={mode === "create" || mode === "edit"}
        segment={target ?? undefined}
      />
      <DeleteSegmentDialog
        onOpenChange={close}
        open={mode === "delete"}
        segment={target}
      />
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/segments/")({
  head: () => ({ meta: [{ title: "Segments · Norbelys" }] }),
  component: SegmentsPage,
});
