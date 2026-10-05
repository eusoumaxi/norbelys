import type { ThreadObject, UpdateThread } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { toast } from "sonner";

import { DialogActions, SubmitButton } from "@/components/dialog-actions";
import { ProblemAlert } from "@/components/problem";
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
import { threadsKey } from "@/features/inbox/queries";
import { useAction } from "@/lib/actions";
import { FormField } from "@/lib/form";
import { fromLocalInput, toLocalInput } from "@/lib/format";
import { fieldProblems, problemLine } from "@/lib/problem";
import { canWrite, useWorkspace } from "@/lib/workspace";

/** 9:00 in the morning, `days` from today. */
const morning = (days: number): Date => {
  const date = new Date();
  date.setDate(date.getDate() + days);
  date.setHours(9, 0, 0, 0);
  return date;
};

/** The days until next Monday: a week on a Monday, never today. */
const untilMonday = (): number => (8 - new Date().getDay()) % 7 || 7;

/** The usual snoozes, at 9:00 so the thread comes back with the working day; counted when asked. */
const presets = () => [
  { days: 1, label: "Tomorrow" },
  { days: untilMonday(), label: "Next Monday" },
  { days: 7, label: "In a week" },
];

/**
 * Puts a thread to sleep until a moment: it reads and filters as snoozed until then, and comes
 * back open by itself (earlier if the person answers).
 */
const SnoozeDialog = ({
  onOpenChange,
  open,
  thread,
}: {
  onOpenChange: (open: boolean) => void;
  open: boolean;
  thread: ThreadObject;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const [until, setUntil] = useState(() => toLocalInput(morning(1)));
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const fieldProblem = fieldProblems(failure).snoozed_until;
  const submit = async () => {
    setBusy(true);
    setFailure(null);
    let done = false;
    try {
      await workspace.api.threads.update(thread.id, {
        snoozed_until: fromLocalInput(until),
        status: "snoozed",
      });
      done = true;
    } catch (error) {
      setFailure(error);
    }
    setBusy(false);
    if (done) {
      toast.success("Conversation snoozed");
      onOpenChange(false);
      await queryClient.invalidateQueries({ queryKey: threadsKey(workspace) });
    }
  };
  return (
    <Dialog
      onOpenChange={(next) => {
        if (!busy) {
          onOpenChange(next);
          setFailure(null);
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
            <DialogTitle>Snooze the conversation</DialogTitle>
            <DialogDescription>
              It leaves the open conversations until then and comes back by
              itself, earlier if they answer.
            </DialogDescription>
          </DialogHeader>
          <DialogBody>
            <div className="flex flex-wrap gap-2">
              {presets().map((preset) => (
                <Button
                  key={preset.label}
                  onClick={() => setUntil(toLocalInput(morning(preset.days)))}
                  size="s"
                  variant="secondary"
                >
                  {preset.label}
                </Button>
              ))}
            </div>
            <FormField
              description="In your time zone."
              htmlFor="snooze-until"
              label="Until"
              problem={fieldProblem}
            >
              <Input
                aria-invalid={Boolean(fieldProblem)}
                id="snooze-until"
                min={toLocalInput(new Date())}
                onChange={(event) => setUntil(event.target.value)}
                required
                type="datetime-local"
                value={until}
              />
            </FormField>
            {failure && !fieldProblem ? (
              <ProblemAlert>{problemLine(failure)}</ProblemAlert>
            ) : null}
          </DialogBody>
          <DialogActions>
            <SubmitButton busy={busy} disabled={!until}>
              Snooze
            </SubmitButton>
          </DialogActions>
        </form>
      </DialogContent>
    </Dialog>
  );
};

/**
 * A thread's header actions (`threads.update`): mark it read or unread, snooze it until a moment,
 * archive it or reopen it. Viewers see none.
 */
export const ThreadActions = ({ thread }: { thread: ThreadObject }) => {
  const workspace = useWorkspace();
  const act = useAction();
  const [snoozing, setSnoozing] = useState(false);
  if (!canWrite(workspace)) {
    return null;
  }
  const update = (label: string, body: UpdateThread) =>
    act(
      label,
      () => workspace.api.threads.update(thread.id, body),
      threadsKey(workspace)
    );
  return (
    <>
      <Button
        onClick={() =>
          update(thread.unread ? "Marked as read" : "Marked as unread", {
            unread: !thread.unread,
          })
        }
        variant="secondary"
      >
        {thread.unread ? "Mark as read" : "Mark as unread"}
      </Button>
      {thread.status === "open" ? (
        <Button onClick={() => setSnoozing(true)} variant="secondary">
          Snooze
        </Button>
      ) : null}
      {thread.status === "archived" ? null : (
        <Button
          onClick={() =>
            update("Conversation archived", { status: "archived" })
          }
          variant="secondary"
        >
          Archive
        </Button>
      )}
      {thread.status === "open" ? null : (
        <Button
          onClick={() => update("Conversation reopened", { status: "open" })}
          variant="primary"
        >
          {thread.status === "snoozed" ? "Unsnooze" : "Reopen"}
        </Button>
      )}
      <SnoozeDialog
        onOpenChange={setSnoozing}
        open={snoozing}
        thread={thread}
      />
    </>
  );
};
