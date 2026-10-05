import type { Combine, SegmentObject } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import type { SubmitEvent } from "react";
import { toast } from "sonner";

import { DialogActions, SubmitButton } from "@/components/dialog-actions";
import { DialogPending, ProblemAlert } from "@/components/problem";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { fieldsQuery, peopleKey } from "@/features/people/queries";
import type { DraftCondition, Subject } from "@/features/segments/filter";
import {
  blankCondition,
  conditionOf,
  draftCondition,
  subjectOf,
  subjectsOf,
} from "@/features/segments/filter";
import { FilterBuilder } from "@/features/segments/filter-builder";
import { segmentKey, segmentsKey } from "@/features/segments/queries";
import { FormField } from "@/lib/form";
import { fieldProblems, problemLine } from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";

/** The conditions a form starts from: the segment's, or one on the address's domain. */
const initialConditions = (
  segment: SegmentObject | undefined,
  subjects: Subject[]
): DraftCondition[] =>
  segment
    ? segment.filter.conditions.map((condition) =>
        draftCondition(condition, subjects)
      )
    : [blankCondition(subjectOf(subjects, "email_domain"))];

/** Whether the API's problems name something the form shows in place. */
const placedIn = (problems: Readonly<Record<string, string>>) =>
  Object.keys(problems).some(
    (path) => path === "name" || path.startsWith("filter")
  );

const SegmentForm = ({
  onSaved,
  segment,
  subjects,
}: {
  onSaved: (segment: SegmentObject) => void;
  segment?: SegmentObject;
  subjects: Subject[];
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const [name, setName] = useState(segment?.name ?? "");
  const [match, setMatch] = useState<Combine>(segment?.filter.match ?? "all");
  const [conditions, setConditions] = useState(() =>
    initialConditions(segment, subjects)
  );
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const problems = fieldProblems(failure);
  const unplaced = failure && !placedIn(problems);

  const submit = async (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    const body = {
      filter: {
        conditions: conditions.map((draft) => conditionOf(draft, subjects)),
        match,
      },
      name: name.trim(),
    };
    try {
      const saved = segment
        ? await workspace.api.segments.update(segment.id, body, {
            headers: { "If-Match": `"${segment.version}"` },
          })
        : await workspace.api.segments.create(body);
      toast.success(segment ? "Segment saved" : "Segment created");
      queryClient.setQueryData(segmentKey(workspace, saved.id), saved);
      void queryClient.invalidateQueries({
        queryKey: [...segmentsKey(workspace), "list"],
      });
      void queryClient.invalidateQueries({
        queryKey: [...segmentsKey(workspace), "options"],
      });
      void queryClient.invalidateQueries({
        queryKey: [...peopleKey(workspace), "list"],
      });
      onSaved(saved);
    } catch (error) {
      setFailure(error);
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
        <FormField htmlFor="segment-name" label="Name" problem={problems.name}>
          <Input
            aria-invalid={Boolean(problems.name)}
            autoFocus
            id="segment-name"
            maxLength={200}
            onChange={(event) => setName(event.target.value)}
            placeholder="Gold accounts outside Gmail"
            value={name}
          />
        </FormField>
        <FilterBuilder
          conditions={conditions}
          match={match}
          onConditionsChange={setConditions}
          onMatchChange={setMatch}
          problems={problems}
          subjects={subjects}
        />
        {unplaced ? <ProblemAlert>{problemLine(failure)}</ProblemAlert> : null}
      </DialogBody>
      <DialogActions note="Who matches is decided each time the segment is read.">
        <SubmitButton
          busy={busy}
          disabled={!name.trim() || conditions.length === 0}
        >
          {segment ? "Save changes" : "Create segment"}
        </SubmitButton>
      </DialogActions>
    </form>
  );
};

/** The form once the custom fields (what conditions can name) are loaded. */
const SegmentFormLoader = ({
  onSaved,
  segment,
}: {
  onSaved: (segment: SegmentObject) => void;
  segment?: SegmentObject;
}) => {
  const workspace = useWorkspace();
  const fields = useQuery(fieldsQuery(workspace));
  if (!fields.isSuccess) {
    return <DialogPending query={fields} />;
  }
  return (
    <SegmentForm
      onSaved={onSaved}
      segment={segment}
      subjects={subjectsOf(fields.data)}
    />
  );
};

/**
 * Creates a segment (`segments.create`) or replaces an existing one's name and filter
 * (`segments.update`, with its version in `If-Match`) through the filter builder. A segment never
 * stores its people: whoever matches the filter when it is read is in it.
 */
export const SegmentDialog = ({
  onOpenChange,
  onSaved,
  open,
  segment,
}: {
  onOpenChange: (open: boolean) => void;
  /** Runs after a save, with the segment as the API returned it. */
  onSaved?: (segment: SegmentObject) => void;
  open: boolean;
  /** The segment to edit; none to create one. */
  segment?: SegmentObject;
}) => (
  <Dialog onOpenChange={onOpenChange} open={open}>
    <DialogContent className="max-w-[880px]">
      <DialogHeader>
        <DialogTitle>{segment ? "Edit segment" : "Create segment"}</DialogTitle>
        <DialogDescription>
          A saved filter over people&apos;s attributes and custom fields, usable
          as a campaign audience or an export.
        </DialogDescription>
      </DialogHeader>
      <SegmentFormLoader
        key={segment ? `${segment.id}:${segment.version}` : "new"}
        onSaved={(saved) => {
          onOpenChange(false);
          onSaved?.(saved);
        }}
        segment={segment}
      />
    </DialogContent>
  </Dialog>
);
