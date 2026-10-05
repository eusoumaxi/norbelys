import type { CampaignObject, PersonObject } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { toast } from "sonner";

import { DialogActions } from "@/components/dialog-actions";
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
import { Select } from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { sendersQuery } from "@/features/campaigns/queries";
import { unsavedContent } from "@/features/campaigns/sequence/draft";
import type {
  StepDraft,
  VariantDraft,
} from "@/features/campaigns/sequence/draft";
import { poolSender, working } from "@/features/campaigns/sequence/senders";
import { messagesKey } from "@/features/messages/queries";
import { PersonPicker } from "@/features/people/person-picker";
import { FormField } from "@/lib/form";
import { formatAddress } from "@/lib/format";
import { problemLine } from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";

/** The sender choice that leaves `from` out: the campaign's pool names its next sender. */
const POOL = "pool";

/**
 * Sends one saved variant of a step to the signed-in person's own address, filled in for a person
 * of the workspace (`messages.create` with `variant_id`, `person_id` and `to`). It is a real email,
 * queued now and sent from the campaign's next sender or the one chosen; it enrolls no one and
 * counts in none of the campaign's results. A campaign whose pool names no sender starts on the
 * first identity of the workspace's mailboxes.
 */
export const SendTestDialog = ({
  base,
  initialPerson,
  onOpenChange,
  open,
  step,
  variant,
}: {
  /** The campaign as saved: the test sends the saved version of the variant. */
  base: CampaignObject;
  initialPerson: PersonObject | null;
  onOpenChange: (open: boolean) => void;
  open: boolean;
  step: StepDraft;
  variant: VariantDraft;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const senders = useQuery({ ...sendersQuery(workspace), enabled: open });
  const [person, setPerson] = useState(initialPerson);
  const [from, setFrom] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  // Each opening starts from the person the preview shows, when it shows one.
  const [shown, setShown] = useState(open);
  if (open !== shown) {
    setShown(open);
    if (open && initialPerson) {
      setPerson(initialPerson);
    }
  }
  const me = workspace.session.me.email;
  const identities = (senders.data ?? []).filter(
    ({ connection, identity }) =>
      identity.enabled && connection.status !== "archived"
  );
  // The campaign's pool when one of its senders can send now, else the first mailbox that can.
  const firstWorking = identities.find(working)?.identity.id;
  const sender =
    from ??
    (poolSender(base, identities) || !firstWorking ? POOL : firstWorking);
  const options = [
    { label: "The campaign's next sender", value: POOL },
    ...identities.map((entry) => ({
      label: working(entry)
        ? formatAddress(entry.identity)
        : `${formatAddress(entry.identity)} (can't send now)`,
      value: entry.identity.id,
    })),
  ];
  const what =
    step.variants.length > 1
      ? `Variant ${variant.name} of “${step.name}”`
      : `“${step.name}”`;

  const send = async () => {
    if (!person || !variant.id) {
      return;
    }
    setBusy(true);
    setFailure(null);
    try {
      const message = await workspace.api.messages.create({
        from: sender === POOL ? undefined : sender,
        person_id: person.id,
        to: me,
        variant_id: variant.id,
      });
      void queryClient.invalidateQueries({ queryKey: messagesKey(workspace) });
      toast.success(`Test queued for ${me}`, {
        action: {
          label: "Open",
          onClick: () => {
            void navigate({
              params: { messageId: message.id, slug: workspace.slug },
              to: "/w/$slug/messages/$messageId",
            });
          },
        },
      });
      onOpenChange(false);
    } catch (error) {
      setFailure(error);
    }
    setBusy(false);
  };

  return (
    <Dialog
      onOpenChange={(next) => {
        onOpenChange(next);
        setFailure(null);
      }}
      open={open}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Send a test</DialogTitle>
          <DialogDescription>
            {what} goes to you at {me}, filled in with the details of the person
            you choose. It doesn&apos;t enroll them or count in the
            campaign&apos;s results.
          </DialogDescription>
        </DialogHeader>
        <DialogBody>
          {unsavedContent(variant, base) ? (
            <p className="text-warning text-xs">
              This variant has unsaved changes: the test sends the saved
              version. Save the sequence first to test them.
            </p>
          ) : null}
          <FormField htmlFor="test-person" label="Written for">
            <PersonPicker
              id="test-person"
              onChange={setPerson}
              value={person}
            />
          </FormField>
          <FormField htmlFor="test-from" label="From">
            <Select
              disabled={senders.isPending}
              id="test-from"
              onChange={setFrom}
              options={options}
              value={sender}
            />
          </FormField>
          {failure ? <ProblemAlert>{problemLine(failure)}</ProblemAlert> : null}
        </DialogBody>
        <DialogActions>
          <Button
            disabled={!person || busy}
            onClick={() => {
              void send();
            }}
            variant="primary"
          >
            {busy ? <Spinner /> : null}
            Send a test
          </Button>
        </DialogActions>
      </DialogContent>
    </Dialog>
  );
};
