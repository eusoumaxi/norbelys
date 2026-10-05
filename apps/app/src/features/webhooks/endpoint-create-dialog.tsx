import type { EndpointObject } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";

import { DialogActions, SubmitButton } from "@/components/dialog-actions";
import { ProblemAlert } from "@/components/problem";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogBody,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { EventTypePicker } from "@/features/webhooks/event-type-picker";
import {
  DEFAULT_EVENT_TYPES,
  toSubscription,
} from "@/features/webhooks/event-types";
import { endpointQuery, endpointsKey } from "@/features/webhooks/queries";
import { SecretReveal } from "@/features/webhooks/secret-dialog";
import { FormField } from "@/lib/form";
import { fieldProblems, problemAt, problemLine } from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";

/**
 * An endpoint's URL and the event types it receives, as its creation and its settings edit them,
 * with the API's problems with either beside it and any other problem of `failure` under them.
 */
export const EndpointFields = ({
  autoFocus = false,
  failure,
  onTypesChange,
  onUrlChange,
  scroll = false,
  types,
  typesDescription,
  url,
}: {
  autoFocus?: boolean;
  failure: unknown;
  onTypesChange: (types: string[]) => void;
  onUrlChange: (url: string) => void;
  /** Bounds the list of types' height, for a dialog. */
  scroll?: boolean;
  types: string[];
  typesDescription?: string;
  url: string;
}) => {
  const problems = fieldProblems(failure);
  const typesProblem = problemAt(problems, "event_types");
  return (
    <>
      <FormField
        description="An https URL on the public internet; redirects are not followed."
        htmlFor="endpoint-url"
        label="URL"
        problem={problems.url}
      >
        <Input
          aria-invalid={Boolean(problems.url)}
          autoFocus={autoFocus}
          className="font-mono"
          id="endpoint-url"
          onChange={(event) => onUrlChange(event.target.value)}
          placeholder="https://example.com/webhooks/norbelys"
          type="url"
          value={url}
        />
      </FormField>
      <FormField
        description={typesDescription}
        label="Events"
        problem={typesProblem}
      >
        <EventTypePicker
          invalid={Boolean(typesProblem)}
          onChange={onTypesChange}
          scroll={scroll}
          value={types}
        />
      </FormField>
      {failure && !problems.url && !typesProblem ? (
        <ProblemAlert>{problemLine(failure)}</ProblemAlert>
      ) : null}
    </>
  );
};

/** The URL and event types of a new endpoint; the API's own validation shows beside each. */
const EndpointForm = ({
  onCreated,
}: {
  onCreated: (endpoint: EndpointObject) => void;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const [url, setUrl] = useState("");
  const [types, setTypes] = useState<string[]>(DEFAULT_EVENT_TYPES);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const submit = async () => {
    setBusy(true);
    setFailure(null);
    try {
      const created = await workspace.api.webhookEndpoints.create({
        event_types: toSubscription(types),
        url: url.trim(),
      });
      queryClient.setQueryData(endpointQuery(workspace, created.id).queryKey, {
        ...created,
        secret: null,
      });
      void queryClient.invalidateQueries({ queryKey: endpointsKey(workspace) });
      onCreated(created);
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
        event.preventDefault();
        void submit();
      }}
    >
      <DialogBody className="gap-5">
        <EndpointFields
          autoFocus
          failure={failure}
          onTypesChange={setTypes}
          onUrlChange={setUrl}
          scroll
          types={types}
          url={url}
        />
      </DialogBody>
      <DialogActions>
        <SubmitButton busy={busy} disabled={!url.trim() || types.length === 0}>
          Add endpoint
        </SubmitButton>
      </DialogActions>
    </form>
  );
};

/** The new endpoint's secret, shown once, and the way to its page. */
const Created = ({ endpoint }: { endpoint: EndpointObject }) => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  return (
    <>
      <DialogBody>
        {endpoint.secret ? <SecretReveal secret={endpoint.secret} /> : null}
      </DialogBody>
      <DialogFooter>
        <DialogClose render={<Button variant="secondary" />}>Done</DialogClose>
        <Button
          onClick={() => {
            void navigate({
              params: { endpointId: endpoint.id, slug: workspace.slug },
              to: "/w/$slug/webhooks/$endpointId",
            });
          }}
          variant="primary"
        >
          Open endpoint
        </Button>
      </DialogFooter>
    </>
  );
};

/**
 * Adds a webhook endpoint: its URL and the event types it receives (the usual eight are ticked,
 * every type is offered). Once created, the same dialog shows the signing secret, the only time
 * the API returns it.
 */
export const EndpointCreateDialog = ({
  onOpenChange,
  open,
}: {
  onOpenChange: (open: boolean) => void;
  open: boolean;
}) => {
  const [created, setCreated] = useState<EndpointObject | null>(null);
  return (
    <Dialog
      onOpenChange={onOpenChange}
      onOpenChangeComplete={(next) => {
        if (!next) {
          setCreated(null);
        }
      }}
      open={open}
    >
      <DialogContent className="max-w-[640px]">
        <DialogHeader>
          <DialogTitle>
            {created ? "Copy the signing secret" : "Add webhook endpoint"}
          </DialogTitle>
          <DialogDescription>
            {created
              ? "The endpoint is live. Every delivery is signed with this secret; verify the signature before trusting a request."
              : "Norbelys POSTs each event it receives as JSON, signed per Standard Webhooks, and retries for about three days until it answers 2xx."}
          </DialogDescription>
        </DialogHeader>
        {created ? (
          <Created endpoint={created} />
        ) : (
          <EndpointForm onCreated={setCreated} />
        )}
      </DialogContent>
    </Dialog>
  );
};
