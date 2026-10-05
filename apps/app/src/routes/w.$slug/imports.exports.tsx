import { Download04Icon, FileExportIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { ExportObject } from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { createStandardSchemaV1, parseAsString } from "nuqs";
import { useState } from "react";
import { toast } from "sonner";

import { Dash, ListTable, NameCell } from "@/components/data-table";
import { CopyIdItem, RowMenu } from "@/components/row-menu";
import { StatusBadge } from "@/components/status-badge";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { Spinner } from "@/components/ui/spinner";
import {
  ExportButton,
  ExportDialog,
  RESOURCES,
} from "@/features/imports/export-dialog";
import {
  exportListQuery,
  exportRunning,
  exportsKey,
} from "@/features/imports/queries";
import { groupOptionsQuery } from "@/features/people/queries";
import { segmentOptionsQuery } from "@/features/segments/queries";
import { useAction } from "@/lib/actions";
import {
  formatCount,
  formatDate,
  formatRelative,
  humanize,
} from "@/lib/format";
import { describeProblem } from "@/lib/problem";
import { canAdminister, useWorkspace } from "@/lib/workspace";

// Declared once for nuqs (the dialog's state) and the router (typed links to `?export=new`).
const search = { export: parseAsString };

/** A resource's name, or its own words for one this dashboard does not know. */
const resourceLabel = (resource: string): string =>
  RESOURCES.find((entry) => entry.value === resource)?.label ??
  humanize(resource);

/** The names of groups and segments, to read an export's filters by. */
const useFilterNames = () => {
  const workspace = useWorkspace();
  const groups = useQuery(groupOptionsQuery(workspace));
  const segments = useQuery(segmentOptionsQuery(workspace));
  return new Map<string, string>([
    ...(groups.data ?? []).map((group): [string, string] => [
      group.id,
      group.name,
    ]),
    ...(segments.data ?? []).map((segment): [string, string] => [
      segment.id,
      segment.name,
    ]),
  ]);
};

/** An export's filters in words: a group, a segment, a range of days, or everything. */
const filterWords = (
  filters: Record<string, unknown>,
  names: Map<string, string>
): string[] => {
  const words: string[] = [];
  for (const [key, value] of Object.entries(filters)) {
    const text = typeof value === "string" ? value : JSON.stringify(value);
    if (key === "group_id" || key === "segment_id") {
      const kind = key === "group_id" ? "Group" : "Segment";
      words.push(`${kind}: ${names.get(text) ?? text}`);
    } else if (key === "created_at[gte]") {
      words.push(`From ${formatDate(text)}`);
    } else if (key === "created_at[lt]") {
      words.push(`Before ${formatDate(text)}`);
    } else {
      words.push(`${key}: ${text}`);
    }
  }
  return words;
};

/** The status, with the reason under it when the export failed. */
const ExportStatus = ({ item }: { item: ExportObject }) => (
  <span className="flex min-w-0 flex-col gap-0.5">
    <StatusBadge kind="job" value={item.status} />
    {item.last_error ? (
      <span
        className="text-fg-3 max-w-56 truncate text-xs"
        title={item.last_error.detail}
      >
        {item.last_error.detail}
      </span>
    ) : null}
  </span>
);

/** When a ready file is deleted; an expired one is gone. */
const Expiry = ({ item }: { item: ExportObject }) => {
  if (item.status === "ready") {
    return <>{formatRelative(item.expires_at)}</>;
  }
  if (item.status === "expired") {
    return <span className="text-fg-3">Expired</span>;
  }
  return <Dash />;
};

/**
 * Downloads a ready file: its link lasts 15 minutes from a read, so the export is read again
 * for a fresh one first.
 */
const DownloadButton = ({ item }: { item: ExportObject }) => {
  const workspace = useWorkspace();
  const [busy, setBusy] = useState(false);
  if (item.status !== "ready") {
    return null;
  }
  const download = async () => {
    setBusy(true);
    try {
      const fresh = await workspace.api.exports.retrieve(item.id);
      if (fresh.url) {
        window.location.assign(fresh.url);
      } else {
        toast.error("The file is no longer available.");
      }
    } catch (error) {
      toast.error(describeProblem(error).detail);
    }
    setBusy(false);
  };
  return (
    <Button
      disabled={busy}
      onClick={() => {
        void download();
      }}
      size="s"
      variant="secondary"
    >
      {busy ? <Spinner /> : <HugeiconsIcon icon={Download04Icon} />}
      Download
    </Button>
  );
};

/** The row's menu: copy the id, or cancel a running export's job (owners and admins). */
const ExportMenu = ({ item }: { item: ExportObject }) => {
  const workspace = useWorkspace();
  const action = useAction();
  const jobId = item.job_id;
  return (
    <RowMenu label="Export actions">
      <CopyIdItem id={item.id} noun="export" />
      {jobId && exportRunning(item) && canAdminister(workspace) ? (
        <DropdownMenuItem
          className="text-error-fg"
          onClick={() =>
            action(
              "Cancellation requested",
              () => workspace.api.jobs.cancel(jobId),
              exportsKey(workspace)
            )
          }
        >
          Cancel export
        </DropdownMenuItem>
      ) : null}
    </RowMenu>
  );
};

/**
 * The exports, newest first (`exports.list`), read again while one runs: what each holds, its
 * filters, its status and rows, and a download while the file is kept (7 days). "New export"
 * (`?export=new`) starts one.
 */
const ExportsPage = () => {
  const workspace = useWorkspace();
  const names = useFilterNames();
  return (
    <>
      <ListTable<ExportObject>
        columns={[
          {
            render: (e) => (
              <NameCell icon={FileExportIcon}>
                {resourceLabel(e.resource)}
              </NameCell>
            ),
            header: "Export",
            id: "resource",
          },
          {
            render: (e) => {
              const words = filterWords(e.filters, names);
              return words.length > 0 ? (
                <span className="text-fg-2 block max-w-64 truncate">
                  {words.join(" · ")}
                </span>
              ) : (
                <span className="text-fg-3">Everything</span>
              );
            },
            header: "Filters",
            id: "filters",
          },
          {
            render: (e) => e.format.toUpperCase(),
            header: "Format",
            id: "format",
          },
          {
            render: (e) => <ExportStatus item={e} />,
            header: "Status",
            id: "status",
          },
          {
            render: (e) =>
              e.rows === null || e.rows === undefined ? (
                <Dash />
              ) : (
                formatCount(e.rows)
              ),
            header: "Rows",
            id: "rows",
          },
          {
            render: (e) => formatRelative(e.created_at),
            header: "Started",
            id: "created",
          },
          {
            render: (e) => <Expiry item={e} />,
            header: "Expires",
            id: "expires",
          },
          {
            render: (e) => <DownloadButton item={e} />,
            className: "w-[120px]",
            header: "",
            id: "download",
          },
          {
            render: (e) => <ExportMenu item={e} />,
            className: "w-[62px]",
            header: "",
            id: "menu",
          },
        ]}
        empty={{
          action: <ExportButton />,
          description:
            "Export people, or the history of messages, attempts, delivery events and inbound messages, as CSV or JSON Lines.",
          icon: FileExportIcon,
          title: "No exports yet",
        }}
        query={exportListQuery(workspace)}
        rowKey={(e) => e.id}
      />
      <ExportDialog />
    </>
  );
};

export const Route = createFileRoute("/w/$slug/imports/exports")({
  validateSearch: createStandardSchemaV1(search, { partialOutput: true }),
  head: () => ({ meta: [{ title: "Exports · Norbelys" }] }),
  component: ExportsPage,
});
