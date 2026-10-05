import type { CreateEnrollments, Enrolled, Skipped } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { toast } from "sonner";

import { DialogActions, SubmitButton } from "@/components/dialog-actions";
import { ProblemAlert } from "@/components/problem";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Segmented } from "@/components/ui/segmented";
import { Select } from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import {
  allGroupsQuery,
  allSegmentsQuery,
  campaignsKey,
  enrollmentsKey,
} from "@/features/campaigns/queries";
import { FormField } from "@/lib/form";
import { formatCount, plural } from "@/lib/format";
import { describeProblem, fieldProblems, problemLine } from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";

type Source = "emails" | "group" | "segment";

/** The addresses typed, one per line or separated by commas or spaces, each once. */
const addresses = (text: string): string[] => [
  ...new Set(
    text
      .split(/[\s,;]+/u)
      .map((part) => part.trim())
      .filter(Boolean)
  ),
];

/** Whether an answer is the enrollments made (`201`), not the job that will make them (`202`). */
const isEnrolled = (answer: unknown): answer is Enrolled =>
  typeof answer === "object" &&
  answer !== null &&
  "skipped" in answer &&
  Array.isArray(answer.skipped);

const SKIP_REASONS: Record<Skipped["reason"], string> = {
  already_enrolled: "already enrolled",
  not_found: "not people of this workspace",
  suppressed: "suppressed",
};

/** `3 already enrolled, 1 suppressed`: why people were skipped, counted by reason. */
const skippedSummary = (skipped: readonly Skipped[]): string | undefined => {
  if (skipped.length === 0) {
    return undefined;
  }
  const counts = new Map<Skipped["reason"], number>();
  for (const skip of skipped) {
    counts.set(skip.reason, (counts.get(skip.reason) ?? 0) + 1);
  }
  return `Skipped: ${[...counts]
    .map(([reason, count]) => `${formatCount(count)} ${SKIP_REASONS[reason]}`)
    .join(", ")}.`;
};

/** The API's error for the chosen source, naming the address it is about when it is one. */
const sourceError = (
  fields: Readonly<Record<string, string>>,
  source: Source,
  emails: readonly string[]
): string | undefined => {
  const prefix = source === "emails" ? "emails" : `${source}_id`;
  const found = Object.entries(fields).find(([path]) =>
    path.startsWith(prefix)
  );
  if (!found) {
    return undefined;
  }
  const [path, message] = found;
  const index = /^emails\[(?<index>\d+)\]/u.exec(path)?.groups?.index;
  const address = index === undefined ? undefined : emails[Number(index)];
  return address ? `${address}: ${message}` : message;
};

/** The control of the chosen source: a box of addresses, or a group or a segment to pick. */
const SourceField = ({
  error,
  onChange,
  source,
  value,
}: {
  error?: string;
  onChange: (value: string) => void;
  source: Source;
  value: string;
}) => {
  const workspace = useWorkspace();
  const groups = useQuery({
    ...allGroupsQuery(workspace),
    enabled: source === "group",
  });
  const segments = useQuery({
    ...allSegmentsQuery(workspace),
    enabled: source === "segment",
  });
  if (source === "emails") {
    return (
      <FormField
        description="One per line, or separated by commas; at most 1,000. Addresses that are not people of this workspace are skipped: add them under People first."
        problem={error}
        htmlFor="enroll-emails"
        label="Addresses"
      >
        <Textarea
          aria-invalid={Boolean(error)}
          autoFocus
          className="min-h-32 font-mono text-xs"
          id="enroll-emails"
          onChange={(event) => onChange(event.target.value)}
          placeholder={"ana@acme.com\nben@globex.com"}
          value={value}
        />
      </FormField>
    );
  }
  const list = source === "group" ? groups : segments;
  const options =
    source === "group"
      ? (groups.data ?? []).map((g) => ({
          label: `${g.name} · ${plural(g.people_count, "person", "people")}`,
          value: g.id,
        }))
      : (segments.data ?? []).map((s) => ({ label: s.name, value: s.id }));
  return (
    <FormField
      description={
        source === "group"
          ? "Everyone in the group now."
          : "Everyone the segment's filter matches now."
      }
      problem={
        error ?? (list.isError ? describeProblem(list.error).detail : undefined)
      }
      htmlFor="enroll-source"
      label={source === "group" ? "Group" : "Segment"}
    >
      <Select
        disabled={list.isPending}
        id="enroll-source"
        onChange={onChange}
        options={options}
        placeholder={list.isPending ? "Loading…" : "Choose…"}
        value={value || null}
      />
    </FormField>
  );
};

/**
 * Enrolls people into the campaign from exactly one source, as `enrollments.create` takes them:
 * addresses, a group or a segment. Up to 100 people are enrolled at once and the people skipped
 * are counted by reason; more are enrolled by a job in the background.
 */
export const EnrollDialog = ({
  campaignId,
  onOpenChange,
  open,
}: {
  campaignId: string;
  onOpenChange: (open: boolean) => void;
  open: boolean;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const [source, setSource] = useState<Source>("emails");
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const fields = fieldProblems(failure);
  const emails = addresses(value);
  const placed = sourceError(fields, source, emails);
  const ready = source === "emails" ? emails.length > 0 : Boolean(value);

  const close = () => {
    onOpenChange(false);
    setValue("");
    setFailure(null);
  };

  const body = (): CreateEnrollments => {
    if (source === "emails") {
      return { campaign_id: campaignId, emails };
    }
    return source === "group"
      ? { campaign_id: campaignId, group_id: value }
      : { campaign_id: campaignId, segment_id: value };
  };

  const submit = async () => {
    setBusy(true);
    setFailure(null);
    try {
      const answer: unknown = await workspace.api.enrollments.create(body());
      if (isEnrolled(answer)) {
        toast.success(`${formatCount(answer.data.length)} enrolled`, {
          description: skippedSummary(answer.skipped),
        });
      } else {
        toast.success("Enrolling in the background", {
          description:
            "More than 100 people: a job enrolls them, and the list fills in as it runs.",
        });
      }
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: enrollmentsKey(workspace) }),
        queryClient.invalidateQueries({ queryKey: campaignsKey(workspace) }),
      ]);
      close();
    } catch (error) {
      setFailure(error);
    }
    setBusy(false);
  };

  return (
    <Dialog
      onOpenChange={(next) => {
        if (next) {
          onOpenChange(true);
        } else {
          close();
        }
      }}
      open={open}
    >
      <DialogContent>
        <form
          className="flex min-h-0 flex-col"
          onSubmit={(event) => {
            event.preventDefault();
            void submit();
          }}
        >
          <DialogHeader>
            <DialogTitle>Enroll people</DialogTitle>
            <DialogDescription>
              Each person starts at the first step. Suppressed addresses and
              people already in this campaign are skipped.
            </DialogDescription>
          </DialogHeader>
          <DialogBody>
            <Segmented
              label="Who to enroll"
              onChange={(next) => {
                setSource(next);
                setValue("");
                setFailure(null);
              }}
              options={[
                { label: "Addresses", value: "emails" },
                { label: "A group", value: "group" },
                { label: "A segment", value: "segment" },
              ]}
              value={source}
            />
            <SourceField
              error={placed}
              onChange={setValue}
              source={source}
              value={value}
            />
            {failure && !placed ? (
              <ProblemAlert>{problemLine(failure)}</ProblemAlert>
            ) : null}
          </DialogBody>
          <DialogActions>
            <SubmitButton busy={busy} disabled={!ready}>
              {source === "emails" && emails.length > 0
                ? `Enroll ${formatCount(emails.length)}`
                : "Enroll"}
            </SubmitButton>
          </DialogActions>
        </form>
      </DialogContent>
    </Dialog>
  );
};
