import { FileImportIcon } from "@hugeicons/core-free-icons";
import type { ImportObject } from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";

import { Dash, ListTable, NameCell } from "@/components/data-table";
import { StatusBadge } from "@/components/status-badge";
import { Badge } from "@/components/ui/badge";
import { ImportButton } from "@/features/imports/import-button";
import { importListQuery } from "@/features/imports/queries";
import { groupOptionsQuery } from "@/features/people/queries";
import { formatCount, formatDateTime, formatRelative } from "@/lib/format";
import { canWrite, useWorkspace } from "@/lib/workspace";

/** Rows not imported draw the eye; none is a quiet zero. */
const Invalid = ({ count }: { count: number }) =>
  count > 0 ? (
    <span className="text-error-fg font-semibold">{formatCount(count)}</span>
  ) : (
    <span className="text-fg-3">0</span>
  );

/**
 * The imports, newest first (`imports.list`), read again while one runs; a row opens its page,
 * and "Import people" opens the import of a file (`/imports/new`).
 */
const ImportsPage = () => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const groups = useQuery(groupOptionsQuery(workspace));
  const names = new Map(
    (groups.data ?? []).map((group) => [group.id, group.name])
  );
  return (
    <ListTable<ImportObject>
      columns={[
        {
          render: (i) => (
            <NameCell icon={FileImportIcon}>
              {formatDateTime(i.created_at)}
            </NameCell>
          ),
          header: "Started",
          id: "started",
        },
        {
          render: (i) => <StatusBadge kind="job" value={i.status} />,
          header: "Status",
          id: "status",
        },
        {
          render: (i) => formatCount(i.counts.total),
          header: "Rows read",
          id: "total",
        },
        {
          render: (i) => formatCount(i.counts.imported),
          header: "Imported",
          id: "imported",
        },
        {
          render: (i) => <Invalid count={i.counts.invalid} />,
          header: "Not imported",
          id: "invalid",
        },
        {
          render: (i) =>
            i.group_id ? (
              <Badge className="max-w-48">
                <span className="truncate">
                  {names.get(i.group_id) ?? "A group"}
                </span>
              </Badge>
            ) : (
              <Dash />
            ),
          header: "Group",
          id: "group",
        },
        {
          render: (i) =>
            i.completed_at ? formatRelative(i.completed_at) : <Dash />,
          header: "Finished",
          id: "finished",
        },
      ]}
      empty={{
        action: canWrite(workspace) ? <ImportButton /> : undefined,
        description:
          "Bring people in from a CSV file, such as a spreadsheet or a CRM export. Norbelys matches its columns to people's details and your fields.",
        icon: FileImportIcon,
        illustration: "people",
        title: "No imports yet",
      }}
      onRowClick={(i) => {
        void navigate({
          params: { importId: i.id, slug: workspace.slug },
          to: "/w/$slug/imports/$importId",
        });
      }}
      query={importListQuery(workspace)}
      rowKey={(i) => i.id}
    />
  );
};

export const Route = createFileRoute("/w/$slug/imports/")({
  head: () => ({ meta: [{ title: "Imports · Norbelys" }] }),
  component: ImportsPage,
});
