import type { ThreadObject } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { toast } from "sonner";

import { ProblemAlert } from "@/components/problem";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardFooter,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { FieldError } from "@/components/ui/field";
import { Spinner } from "@/components/ui/spinner";
import { threadsKey } from "@/features/inbox/queries";
import {
  BodyEditor,
  bodyHtml,
  EMPTY_BODY,
} from "@/features/messages/body-editor";
import type { BodyDraft } from "@/features/messages/body-editor";
import { messagesKey } from "@/features/messages/queries";
import { fieldProblems, problemLine } from "@/lib/problem";
import { canWrite, useWorkspace } from "@/lib/workspace";

/** Delivery and abuse reports: a reply never answers a mailer daemon or a feedback loop. */
const REPORTS = new Set(["bounce", "complaint"]);

/**
 * Who a reply without `to` goes to, as the API decides it: the sender of the thread's latest
 * inbound message that is not a delivery or abuse report. `null` when none is shown.
 */
const replyTarget = (thread: ThreadObject): string | null =>
  (thread.messages?.data ?? [])
    .toReversed()
    .find(
      (entry) =>
        entry.direction === "inbound" &&
        !REPORTS.has(entry.classification ?? "")
    )?.from ?? null;

/**
 * An answer in the conversation (`messages.create` with `thread_id`): sent from the thread's own
 * sender identity to whoever wrote last, as `Re:` and the thread's subject, queued at once and
 * sent within the mailbox's pacing. Shown only once someone has written back.
 */
export const ReplyBox = ({ thread }: { thread: ThreadObject }) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const [draft, setDraft] = useState<BodyDraft>(EMPTY_BODY);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const to = replyTarget(thread);
  if (!canWrite(workspace) || !to) {
    return null;
  }
  const fieldProblem = fieldProblems(failure).html;
  const submit = async () => {
    setBusy(true);
    setFailure(null);
    let sent = false;
    try {
      await workspace.api.messages.create({
        html: bodyHtml(draft),
        thread_id: thread.id,
      });
      sent = true;
    } catch (error) {
      setFailure(error);
    }
    setBusy(false);
    if (sent) {
      setDraft(EMPTY_BODY);
      toast.success("Reply queued");
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: threadsKey(workspace) }),
        queryClient.invalidateQueries({ queryKey: messagesKey(workspace) }),
      ]);
    }
  };
  return (
    <Card>
      <form
        onSubmit={(event) => {
          event.preventDefault();
          void submit();
        }}
      >
        <CardHeader className="flex-col items-start gap-0.5">
          <CardTitle>Reply</CardTitle>
          <CardDescription className="text-xs">
            To {to}, from this conversation’s sender, as a reply to their
            message.
          </CardDescription>
        </CardHeader>
        <CardContent className="flex flex-col gap-2">
          <BodyEditor
            draft={draft}
            id="reply-body"
            invalid={Boolean(fieldProblem)}
            onChange={setDraft}
            placeholder="Write your reply…"
          />
          {fieldProblem ? <FieldError>{fieldProblem}</FieldError> : null}
          {failure && !fieldProblem ? (
            <ProblemAlert>{problemLine(failure)}</ProblemAlert>
          ) : null}
        </CardContent>
        <CardFooter className="justify-end">
          <Button
            disabled={busy || bodyHtml(draft) === ""}
            type="submit"
            variant="primary"
          >
            {busy ? <Spinner /> : null}
            Send reply
          </Button>
        </CardFooter>
      </form>
    </Card>
  );
};
