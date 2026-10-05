import { Alert02Icon, Download04Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { FieldObject, ImportObject } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";

import { MetricGroup } from "@/components/details";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { groupsKey } from "@/features/groups/queries";
import { ATTRIBUTES } from "@/features/imports/csv";
import { ImportButton } from "@/features/imports/import-button";
import { importRunning, importsKey } from "@/features/imports/queries";
import {
  fieldsQuery,
  groupOptionsQuery,
  peopleKey,
} from "@/features/people/queries";
import { useAction } from "@/lib/actions";
import {
  formatCount,
  formatDateTime,
  formatRelative,
  plural,
} from "@/lib/format";
import { canAdminister, canWrite, useWorkspace } from "@/lib/workspace";

/** A problem's column in words: one of the person's details, a field's label, or the whole row. */
const columnWords = (field: string, fields: readonly FieldObject[]): string => {
  if (!field) {
    return "Whole row";
  }
  const attribute = ATTRIBUTES.find((entry) => entry.value === field);
  if (attribute) {
    return attribute.label;
  }
  const key = field.startsWith("fields.")
    ? field.slice("fields.".length)
    : field;
  return fields.find((entry) => entry.key === key)?.label ?? field;
};

/** The API's words for a problem as a sentence: its first letter up, code quotes left out. */
const sentence = (problem: string): string => {
  const text = problem.replaceAll("`", "");
  return text.charAt(0).toUpperCase() + text.slice(1);
};

/**
 * Refreshes what an import changes (the people, their groups, the imports) once an import seen
 * running ends, so the lists show the people it brought in.
 */
const useRefreshWhenDone = (item: ImportObject | undefined) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const running = item ? importRunning(item) : undefined;
  const watched = useRef(false);
  useEffect(() => {
    if (running) {
      watched.current = true;
    } else if (running === false && watched.current) {
      watched.current = false;
      for (const queryKey of [
        peopleKey(workspace),
        groupsKey(workspace),
        importsKey(workspace),
      ]) {
        void queryClient.invalidateQueries({ queryKey });
      }
    }
  }, [queryClient, running, workspace]);
};

/** The import's state in one line: its status, when it started, and the group people join. */
export const ImportStatusLine = ({ item }: { item: ImportObject }) => {
  const workspace = useWorkspace();
  const groups = useQuery(groupOptionsQuery(workspace));
  const group = item.group_id
    ? (groups.data ?? []).find((entry) => entry.id === item.group_id)
    : undefined;
  return (
    <span className="flex flex-wrap items-center gap-x-3 gap-y-1">
      <StatusBadge kind="job" value={item.status} />
      <span className="text-fg-3">
        Started {formatDateTime(item.created_at)}
        {item.group_id ? ` · Added to ${group?.name ?? "a group"}` : null}
      </span>
    </span>
  );
};

/** Cancels the import's job, for owners and admins while it runs. */
const CancelImport = ({ item }: { item: ImportObject }) => {
  const workspace = useWorkspace();
  const action = useAction();
  const [busy, setBusy] = useState(false);
  if (!(item.job_id && importRunning(item) && canAdminister(workspace))) {
    return null;
  }
  const jobId = item.job_id;
  const cancel = async () => {
    setBusy(true);
    await action(
      "Cancellation requested",
      () => workspace.api.jobs.cancel(jobId),
      importsKey(workspace)
    );
    setBusy(false);
  };
  return (
    <Button
      disabled={busy}
      onClick={() => {
        void cancel();
      }}
      variant="danger-secondary"
    >
      {busy ? <Spinner /> : null}
      Cancel import
    </Button>
  );
};

/**
 * The import page's actions: cancelling while it runs (owners and admins); once it ended, the
 * people it brought in (the group's, when it had one) and another import.
 */
export const ImportActions = ({ item }: { item: ImportObject }) => {
  const workspace = useWorkspace();
  if (importRunning(item)) {
    return <CancelImport item={item} />;
  }
  const writer = canWrite(workspace);
  if (item.status !== "completed") {
    return writer ? <ImportButton /> : null;
  }
  return (
    <>
      {writer ? <ImportButton variant="secondary" /> : null}
      <Button
        nativeButton={false}
        render={
          <Link
            params={{ slug: workspace.slug }}
            search={item.group_id ? { group: item.group_id } : {}}
            to="/w/$slug/people"
          />
        }
        variant="primary"
      >
        View people
      </Button>
    </>
  );
};

/** Where the import stands, in a sentence. */
const Progress = ({ item, rows }: { item: ImportObject; rows?: number }) => {
  if (importRunning(item)) {
    let read = `${plural(item.counts.total, "row")} read so far`;
    if (rows !== undefined && rows >= item.counts.total) {
      read = `${formatCount(item.counts.total)} of ${plural(rows, "row")} read`;
    }
    return (
      <p className="text-fg-2 flex items-center gap-2 text-sm">
        <Spinner className="text-fg-3" />
        {item.status === "queued"
          ? "Waiting to start. The counts update as rows are read."
          : `Importing: ${read}.`}
      </p>
    );
  }
  if (item.status === "completed") {
    return (
      <p className="text-fg-2 text-sm">
        Finished
        {item.completed_at
          ? ` ${formatRelative(item.completed_at).toLowerCase()}`
          : null}
        . Each imported row added a person, or updated the one with the same
        email.
      </p>
    );
  }
  return null;
};

/** The rows that were not imported: the row, the column, and what is wrong, in the API's words. */
const RowProblems = ({
  fields,
  item,
}: {
  fields: readonly FieldObject[];
  item: ImportObject;
}) => {
  const problems = item.errors.data.map((problem) => ({
    ...problem,
    id: `${problem.row}:${problem.field}:${problem.problem}`,
  }));
  if (problems.length === 0) {
    return null;
  }
  return (
    <section className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex flex-col gap-1">
          <h2 className="text-fg text-xl font-semibold">Rows not imported</h2>
          <p className="text-fg-2 text-sm">
            {item.errors.has_more
              ? `The first ${formatCount(problems.length)} problems. The report lists every one.`
              : "Fix these rows in your file and import it again: rows already imported are updated, not repeated."}
          </p>
        </div>
        {item.errors.url ? (
          <Button
            nativeButton={false}
            render={
              <a
                aria-label="Download report"
                download
                href={item.errors.url}
                rel="noreferrer"
              />
            }
            variant="secondary"
          >
            <HugeiconsIcon icon={Download04Icon} />
            Download report
          </Button>
        ) : null}
      </div>
      <div className="max-h-[480px] overflow-y-auto">
        <Table shell>
          <TableHeader>
            <TableRow>
              <TableHead className="w-20">Row</TableHead>
              <TableHead className="w-48">Column</TableHead>
              <TableHead>Problem</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {problems.map((problem) => (
              <TableRow key={problem.id}>
                <TableCell className="tabular-nums">{problem.row}</TableCell>
                <TableCell>{columnWords(problem.field, fields)}</TableCell>
                <TableCell className="text-fg-2">
                  {sentence(problem.problem)}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </div>
    </section>
  );
};

/**
 * How one import went, as `imports.retrieve` reads it (read again every 2 s while it is queued or
 * processing): where it stands, its counts (rows read, imported, skipped as repeats of an earlier
 * row, not imported), why it failed when it did, and the rows not imported with the link to the
 * full report once it completed. Rows already imported stay when it is cancelled.
 */
export const ImportReport = ({
  item,
  rows,
}: {
  item: ImportObject;
  /**
   * The rows of the uploaded file, when the page was opened from the import of that file: the
   * API reports the rows read so far, not how many there are.
   */
  rows?: number;
}) => {
  const workspace = useWorkspace();
  const fields = useQuery(fieldsQuery(workspace));
  useRefreshWhenDone(item);
  const { imported, invalid, skipped, total } = item.counts;
  return (
    <div className="flex max-w-[880px] flex-col gap-6">
      <Progress item={item} rows={rows} />
      <MetricGroup
        footer={
          skipped > 0
            ? "A repeat is a row whose email an earlier row of the file already had: the first one counts."
            : undefined
        }
        metrics={[
          { label: "Rows read", value: formatCount(total) },
          { label: "Imported", value: formatCount(imported) },
          { label: "Repeats skipped", value: formatCount(skipped) },
          { label: "Not imported", value: formatCount(invalid) },
        ]}
      />
      {item.last_error ? (
        <Alert variant="error">
          <HugeiconsIcon icon={Alert02Icon} />
          <AlertTitle>The import stopped</AlertTitle>
          <AlertDescription>
            {sentence(item.last_error.detail).replace(/\.?$/u, ".")}{" "}
            {imported > 0
              ? `The ${plural(imported, "person", "people")} imported before it stopped stay in your workspace.`
              : "Nobody was imported."}
          </AlertDescription>
        </Alert>
      ) : null}
      <RowProblems fields={fields.data ?? []} item={item} />
    </div>
  );
};
