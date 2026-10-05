import type {
  InboundClassification,
  InboundMessageObject,
  UpdateInbound,
} from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";

import { StatusBadge } from "@/components/status-badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardFooter,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Segmented } from "@/components/ui/segmented";
import { Select } from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import {
  classificationOptions,
  SOURCES,
} from "@/features/inbox/classification";
import {
  CLASSIFICATIONS,
  inboundKey,
  threadsKey,
} from "@/features/inbox/queries";
import { useAction } from "@/lib/actions";
import { FormField } from "@/lib/form";
import { canWrite, useWorkspace } from "@/lib/workspace";

type SentimentChoice = "positive" | "neutral" | "negative" | "none";

const SENTIMENTS: { label: string; value: SentimentChoice }[] = [
  { label: "Positive", value: "positive" },
  { label: "Neutral", value: "neutral" },
  { label: "Negative", value: "negative" },
  { label: "Not set", value: "none" },
];

const CLASSIFICATION_OPTIONS = classificationOptions();

/** The members that changed, as `inbound_messages.update` takes them (`null` clears the sentiment). */
const changes = (
  message: InboundMessageObject,
  classification: InboundClassification,
  sentiment: SentimentChoice
): UpdateInbound => {
  const body: UpdateInbound = {};
  if (classification !== message.classification) {
    body.classification = classification;
  }
  if (sentiment !== (message.sentiment ?? "none")) {
    body.sentiment = sentiment === "none" ? null : sentiment;
  }
  return body;
};

/** Who decided the classification, with the header or field that decided it. */
const Decided = ({ message }: { message: InboundMessageObject }) => (
  <dl className="grid grid-cols-[98px_1fr] gap-x-2 gap-y-2 text-sm">
    <dt className="text-fg-3 text-xs font-medium">Now</dt>
    <dd className="flex flex-wrap items-center gap-2">
      <StatusBadge kind="classification" value={message.classification} />
      {message.sentiment ? (
        <StatusBadge dot={false} kind="sentiment" value={message.sentiment} />
      ) : null}
    </dd>
    <dt className="text-fg-3 text-xs font-medium">Decided by</dt>
    <dd className="text-fg-2 text-xs">
      {SOURCES[message.classification_source]}
    </dd>
    <dt className="text-fg-3 text-xs font-medium">Evidence</dt>
    <dd className="text-fg font-mono text-xs break-all">{message.evidence}</dd>
  </dl>
);

/**
 * How the message was classified, by whom and on what evidence, and the correction of its
 * classification and sentiment (`inbound_messages.update`). A correction is marked as made by
 * hand, and the AI never overrides it afterwards. Give it `key={message.version}` so the choices
 * start again from each new version.
 */
export const ClassificationCard = ({
  message,
}: {
  message: InboundMessageObject;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const action = useAction();
  const [classification, setClassification] = useState<InboundClassification>(
    message.classification
  );
  const [sentiment, setSentiment] = useState<SentimentChoice>(
    SENTIMENTS.find((option) => option.value === message.sentiment)?.value ??
      "none"
  );
  const [busy, setBusy] = useState(false);
  const body = changes(message, classification, sentiment);
  const changed = Object.keys(body).length > 0;
  const save = async () => {
    setBusy(true);
    await action(
      "Classification corrected",
      () => workspace.api.inboundMessages.update(message.id, body),
      () =>
        Promise.all([
          queryClient.invalidateQueries({ queryKey: inboundKey(workspace) }),
          queryClient.invalidateQueries({ queryKey: threadsKey(workspace) }),
        ])
    );
    setBusy(false);
  };
  return (
    <Card>
      <CardHeader className="flex-col items-start gap-0.5">
        <CardTitle>Classification</CardTitle>
        <CardDescription className="text-xs">
          A correction is yours: it is marked as made by hand, and the AI never
          overrides it afterwards.
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-4">
        <Decided message={message} />
        {canWrite(workspace) ? (
          <div className="border-line flex flex-col gap-4 border-t pt-4">
            <FormField htmlFor="correct-classification" label="Classification">
              <Select
                className="max-w-64"
                id="correct-classification"
                onChange={(next) =>
                  setClassification(
                    CLASSIFICATIONS.find((value) => value === next) ??
                      message.classification
                  )
                }
                options={CLASSIFICATION_OPTIONS}
                value={classification}
              />
            </FormField>
            <FormField label="Sentiment">
              <Segmented<SentimentChoice>
                label="Sentiment"
                onChange={setSentiment}
                options={SENTIMENTS}
                value={sentiment}
              />
            </FormField>
          </div>
        ) : null}
      </CardContent>
      {canWrite(workspace) ? (
        <CardFooter className="justify-end">
          <Button
            disabled={busy || !changed}
            onClick={() => {
              void save();
            }}
            variant="primary"
          >
            {busy ? <Spinner /> : null}
            Save correction
          </Button>
        </CardFooter>
      ) : null}
    </Card>
  );
};
