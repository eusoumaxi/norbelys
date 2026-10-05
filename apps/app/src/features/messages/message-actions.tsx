import type { MessageObject } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import type { ReactNode } from "react";
import { toast } from "sonner";

import { ConfirmDialog } from "@/components/confirm-dialog";
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
import { Segmented } from "@/components/ui/segmented";
import { Textarea } from "@/components/ui/textarea";
import { messagesKey } from "@/features/messages/queries";
import { useAction } from "@/lib/actions";
import { FormField } from "@/lib/form";
import { fieldProblems, problemLine } from "@/lib/problem";
import { canWrite, useWorkspace } from "@/lib/workspace";

/** The most characters of evidence the API keeps. */
const EVIDENCE_MAX = 2000;

/**
 * A decision that needs the evidence behind it, for the record: a text the person must write
 * (what they checked), any other choice above it, and the API's answer when it refuses. The dialog
 * stays open on failure and closes once `onSubmit` succeeds.
 */
const EvidenceDialog = ({
  children,
  description,
  evidenceHint,
  onOpenChange,
  onSubmit,
  open,
  submitLabel,
  title,
}: {
  children?: ReactNode;
  description: ReactNode;
  evidenceHint: string;
  onOpenChange: (open: boolean) => void;
  onSubmit: (evidence: string) => Promise<unknown>;
  open: boolean;
  submitLabel: string;
  title: string;
}) => {
  const [evidence, setEvidence] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const fieldProblem = fieldProblems(failure).evidence;
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
          onSubmit={async (event) => {
            event.preventDefault();
            setBusy(true);
            setFailure(null);
            let done = false;
            try {
              await onSubmit(evidence.trim());
              done = true;
            } catch (error) {
              setFailure(error);
            }
            setBusy(false);
            if (done) {
              setEvidence("");
              onOpenChange(false);
            }
          }}
        >
          <DialogHeader>
            <DialogTitle>{title}</DialogTitle>
            <DialogDescription>{description}</DialogDescription>
          </DialogHeader>
          <DialogBody>
            {children}
            <FormField
              description={evidenceHint}
              htmlFor="evidence"
              label="Evidence"
              problem={fieldProblem}
            >
              <Textarea
                aria-invalid={Boolean(fieldProblem)}
                autoFocus
                id="evidence"
                maxLength={EVIDENCE_MAX}
                onChange={(event) => setEvidence(event.target.value)}
                required
                value={evidence}
              />
            </FormField>
            {failure && !fieldProblem ? (
              <ProblemAlert>{problemLine(failure)}</ProblemAlert>
            ) : null}
          </DialogBody>
          <DialogActions>
            <SubmitButton busy={busy} disabled={!evidence.trim()}>
              {submitLabel}
            </SubmitButton>
          </DialogActions>
        </form>
      </DialogContent>
    </Dialog>
  );
};

type Resolution = "sent" | "failed";

const RESOLUTIONS = [
  { label: "It was sent", value: "sent" as const },
  { label: "It was not sent", value: "failed" as const },
];

/** Settles an uncertain message as sent or failed, with what shows it. */
const ResolveDialog = ({
  message,
  onOpenChange,
  open,
}: {
  message: MessageObject;
  onOpenChange: (open: boolean) => void;
  open: boolean;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const [state, setState] = useState<Resolution>("sent");
  return (
    <EvidenceDialog
      description="Its submission ended without a readable answer, so it is never sent again on its own. Say what happened: the decision is recorded as a manual delivery event, the counters follow and your webhooks hear of it."
      evidenceHint="Where you found the message (the recipient's server, the provider's log), or why it was not sent. At most 2,000 characters."
      onOpenChange={onOpenChange}
      onSubmit={async (evidence) => {
        await workspace.api.messages.resolve(message.id, { evidence, state });
        toast.success(
          state === "sent"
            ? "Message resolved as sent"
            : "Message resolved as failed"
        );
        await queryClient.invalidateQueries({
          queryKey: messagesKey(workspace),
        });
      }}
      open={open}
      submitLabel="Resolve"
      title="Resolve the message"
    >
      <FormField label="What happened">
        <Segmented<Resolution>
          label="What happened"
          onChange={setState}
          options={RESOLUTIONS}
          value={state}
        />
      </FormField>
    </EvidenceDialog>
  );
};

/** Lifts every open hold of the message, so mail to those addresses flows again. */
const ReleaseDialog = ({
  message,
  onOpenChange,
  open,
}: {
  message: MessageObject;
  onOpenChange: (open: boolean) => void;
  open: boolean;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  return (
    <EvidenceDialog
      description="Every open hold of this message is resolved as manual, and mail to those addresses flows again. Your evidence is kept in the workspace’s audit log."
      evidenceHint="What shows the holds no longer apply: what you checked. At most 2,000 characters."
      onOpenChange={onOpenChange}
      onSubmit={async (evidence) => {
        await workspace.api.messages.releaseHolds(message.id, { evidence });
        toast.success("Holds released");
        await queryClient.invalidateQueries({
          queryKey: messagesKey(workspace),
        });
      }}
      open={open}
      submitLabel="Release holds"
      title="Release holds"
    />
  );
};

/** Cancels a queued message after asking. */
const CancelDialog = ({
  message,
  onOpenChange,
  open,
}: {
  message: MessageObject;
  onOpenChange: (open: boolean) => void;
  open: boolean;
}) => {
  const workspace = useWorkspace();
  const action = useAction();
  return (
    <ConfirmDialog
      confirmLabel="Cancel message"
      danger
      description="It will not be sent, and your webhooks hear message.cancelled. Only a queued message can be cancelled: once a sender has claimed it, it is on its way."
      onConfirm={() =>
        action(
          "Message cancelled",
          () => workspace.api.messages.cancel(message.id),
          messagesKey(workspace)
        )
      }
      onOpenChange={onOpenChange}
      open={open}
      title="Cancel this message?"
    />
  );
};

type Opened = "cancel" | "release" | "resolve" | null;

/**
 * The message's header actions, each offered only when it applies: cancel while it is queued,
 * release its holds while some are open, resolve it while it is uncertain. Viewers see none.
 */
export const MessageActions = ({ message }: { message: MessageObject }) => {
  const workspace = useWorkspace();
  const [opened, setOpened] = useState<Opened>(null);
  if (!canWrite(workspace)) {
    return null;
  }
  const close = (open: boolean) => {
    if (!open) {
      setOpened(null);
    }
  };
  const holding = message.holds.some((hold) => !hold.resolved_at);
  return (
    <>
      {message.state === "uncertain" ? (
        <Button onClick={() => setOpened("resolve")} variant="primary">
          Resolve
        </Button>
      ) : null}
      {holding ? (
        <Button onClick={() => setOpened("release")} variant="secondary">
          Release holds
        </Button>
      ) : null}
      {message.state === "queued" ? (
        <Button onClick={() => setOpened("cancel")} variant="danger-secondary">
          Cancel message
        </Button>
      ) : null}
      <ResolveDialog
        message={message}
        onOpenChange={close}
        open={opened === "resolve"}
      />
      <ReleaseDialog
        message={message}
        onOpenChange={close}
        open={opened === "release"}
      />
      <CancelDialog
        message={message}
        onOpenChange={close}
        open={opened === "cancel"}
      />
    </>
  );
};
