import { Alert02Icon, FileExportIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type {
  ExportFormat as Format,
  ExportResource as Resource,
} from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import type { SubmitEvent } from "react";
import { toast } from "sonner";

import { DialogActions, SubmitButton } from "@/components/dialog-actions";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Segmented } from "@/components/ui/segmented";
import { Select } from "@/components/ui/select";
import { exportsKey } from "@/features/imports/queries";
import { groupOptionsQuery } from "@/features/people/queries";
import { segmentOptionsQuery } from "@/features/segments/queries";
import { useUrlDialog } from "@/hooks/use-url-dialog";
import { FormField } from "@/lib/form";
import { problemLine } from "@/lib/problem";
import { canWrite, useWorkspace } from "@/lib/workspace";

/** `?export=new` opens the dialog. */
const NEW_EXPORT = "new";

/** "New export": opens the dialog that starts one (`?export=new`). */
export const ExportButton = () => {
  const dialog = useUrlDialog("export");
  return (
    <Button onClick={() => dialog.open(NEW_EXPORT)} variant="primary">
      <HugeiconsIcon icon={FileExportIcon} />
      New export
    </Button>
  );
};

/** What an export can hold, and what each file's rows are. */
export const RESOURCES: {
  description: string;
  label: string;
  value: Resource;
}[] = [
  {
    description:
      "One row per person: address, names, company, custom fields and groups.",
    label: "People",
    value: "people",
  },
  {
    description: "Every message sent or queued, with its state.",
    label: "Messages",
    value: "messages",
  },
  {
    description:
      "Every attempt to hand a message to a server, with its outcome.",
    label: "Attempts",
    value: "attempts",
  },
  {
    description: "What servers and providers reported about each message.",
    label: "Delivery events",
    value: "delivery_events",
  },
  {
    description: "Every message the inbox read, with its classification.",
    label: "Inbound messages",
    value: "inbound_messages",
  },
];

const FORMATS: { label: string; value: Format }[] = [
  { label: "CSV", value: "csv" },
  { label: "JSON Lines", value: "jsonl" },
];

/** Midnight at the start of a `YYYY-MM-DD` day in the browser's zone, `days` later, as RFC 3339. */
const startOfDay = (day: string, days = 0): string => {
  const [year = 0, month = 1, date = 1] = day.split("-").map(Number);
  return new Date(year, month - 1, date + days).toISOString();
};

/** People narrowed to a group or a segment (both optional). */
const PeopleFilters = ({
  groupId,
  onGroupChange,
  onSegmentChange,
  segmentId,
}: {
  groupId: string;
  onGroupChange: (value: string) => void;
  onSegmentChange: (value: string) => void;
  segmentId: string;
}) => {
  const workspace = useWorkspace();
  const groups = useQuery(groupOptionsQuery(workspace));
  const segments = useQuery(segmentOptionsQuery(workspace));
  return (
    <div className="grid gap-4 sm:grid-cols-2">
      <FormField htmlFor="export-group" label="Group">
        <Select
          id="export-group"
          onChange={onGroupChange}
          options={[
            { label: "Any group", value: "" },
            ...(groups.data ?? []).map((group) => ({
              label: group.name,
              value: group.id,
            })),
          ]}
          value={groupId}
        />
      </FormField>
      <FormField htmlFor="export-segment" label="Segment">
        <Select
          id="export-segment"
          onChange={onSegmentChange}
          options={[
            { label: "Any segment", value: "" },
            ...(segments.data ?? []).map((segment) => ({
              label: segment.name,
              value: segment.id,
            })),
          ]}
          value={segmentId}
        />
      </FormField>
    </div>
  );
};

/** History narrowed to the days it was created on (both ends optional, inclusive). */
const DayRange = ({
  from,
  onFromChange,
  onUntilChange,
  until,
}: {
  from: string;
  onFromChange: (value: string) => void;
  onUntilChange: (value: string) => void;
  until: string;
}) => (
  <div className="grid gap-4 sm:grid-cols-2">
    <FormField htmlFor="export-from" label="From">
      <Input
        id="export-from"
        onChange={(event) => onFromChange(event.target.value)}
        type="date"
        value={from}
      />
    </FormField>
    <FormField htmlFor="export-until" label="Until">
      <Input
        id="export-until"
        onChange={(event) => onUntilChange(event.target.value)}
        type="date"
        value={until}
      />
    </FormField>
  </div>
);

interface Choices {
  resource: Resource;
  groupId: string;
  segmentId: string;
  from: string;
  until: string;
}

/** The filters of the resource's list that the choices name; empty ones are left out. */
const filtersOf = (choices: Choices): Record<string, string> => {
  if (choices.resource === "people") {
    return {
      ...(choices.groupId ? { group_id: choices.groupId } : {}),
      ...(choices.segmentId ? { segment_id: choices.segmentId } : {}),
    };
  }
  return {
    ...(choices.from ? { "created_at[gte]": startOfDay(choices.from) } : {}),
    ...(choices.until
      ? { "created_at[lt]": startOfDay(choices.until, 1) }
      : {}),
  };
};

const ExportForm = ({ onDone }: { onDone: () => void }) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  // Exporting people takes `people:write`, which viewers lack; the history is theirs to export.
  const resources = RESOURCES.filter(
    (entry) => entry.value !== "people" || canWrite(workspace)
  );
  const [choices, setChoices] = useState<Choices>({
    from: "",
    groupId: "",
    resource: resources[0]?.value ?? "messages",
    segmentId: "",
    until: "",
  });
  const [format, setFormat] = useState<Format>("csv");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const choose = (change: Partial<Choices>) =>
    setChoices((current) => ({ ...current, ...change }));
  const description = RESOURCES.find(
    (entry) => entry.value === choices.resource
  )?.description;

  const submit = async (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      await workspace.api.exports.create({
        filters: filtersOf(choices),
        format,
        resource: choices.resource,
      });
      toast.success("Export started");
      void queryClient.invalidateQueries({ queryKey: exportsKey(workspace) });
      onDone();
    } catch (error) {
      setFailure(problemLine(error));
    }
    setBusy(false);
  };

  return (
    <form
      className="contents"
      noValidate
      onSubmit={(event) => {
        void submit(event);
      }}
    >
      <DialogBody className="gap-5">
        <FormField
          description={description}
          htmlFor="export-resource"
          label="What to export"
        >
          <Select
            id="export-resource"
            onChange={(value) => choose({ resource: value as Resource })}
            options={resources.map((entry) => ({
              label: entry.label,
              value: entry.value,
            }))}
            value={choices.resource}
          />
        </FormField>
        {choices.resource === "people" ? (
          <PeopleFilters
            groupId={choices.groupId}
            onGroupChange={(groupId) => choose({ groupId })}
            onSegmentChange={(segmentId) => choose({ segmentId })}
            segmentId={choices.segmentId}
          />
        ) : (
          <DayRange
            from={choices.from}
            onFromChange={(from) => choose({ from })}
            onUntilChange={(until) => choose({ until })}
            until={choices.until}
          />
        )}
        <FormField label="Format">
          <Segmented
            label="Format"
            onChange={setFormat}
            options={FORMATS}
            value={format}
          />
        </FormField>
        {failure ? (
          <Alert variant="error">
            <HugeiconsIcon icon={Alert02Icon} />
            <AlertTitle>Not started</AlertTitle>
            <AlertDescription>{failure}</AlertDescription>
          </Alert>
        ) : null}
      </DialogBody>
      <DialogActions note="The file is kept for 7 days.">
        <SubmitButton busy={busy}>Start export</SubmitButton>
      </DialogActions>
    </form>
  );
};

/**
 * Starts an export (`exports.create`) of a resource's list, as CSV or JSON Lines: people narrowed
 * to a group or a segment, or history narrowed to the days it was created on (archived periods
 * included). The export runs as a job; its row in the list shows when the file is ready. Opened
 * by the `export` search parameter.
 */
export const ExportDialog = () => {
  const dialog = useUrlDialog("export");
  return (
    <Dialog {...dialog.props}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>New export</DialogTitle>
          <DialogDescription>
            A file of a list, made in the background. Download it from this page
            once it is ready.
          </DialogDescription>
        </DialogHeader>
        <ExportForm onDone={() => dialog.close()} />
      </DialogContent>
    </Dialog>
  );
};
