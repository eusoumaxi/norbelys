import type { CampaignObject, PersonObject } from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";
import { cn } from "cn";
import { useState } from "react";

import { Segmented } from "@/components/ui/segmented";
import { sendersQuery } from "@/features/campaigns/queries";
import type {
  StepDraft,
  VariantDraft,
} from "@/features/campaigns/sequence/draft";
import { PieceText } from "@/features/campaigns/sequence/parts";
import { defaultSender } from "@/features/campaigns/sequence/senders";
import { signatureHtml } from "@/features/mailboxes/signature";
import { MailFrame } from "@/features/messages/body-editor";
import { pathLabel } from "@/features/messages/merge-tags";
import type { FieldName } from "@/features/messages/merge-tags";
import {
  missingPaths,
  piecesToHtml,
  renderTemplate,
  unsupportedTags,
} from "@/features/messages/templates";
import type { PreviewContext } from "@/features/messages/templates";
import { PersonPicker, personLabel } from "@/features/people/person-picker";
import { formatAddress } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/** The person a preview shows when none of the workspace's is picked. */
const SAMPLE_PERSON = {
  company: "Analytical Engines",
  email: "ada@example.com",
  family_name: "Lovelace",
  given_name: "Ada",
};

type Device = "desktop" | "mobile";

const DEVICES: { label: string; value: Device }[] = [
  { label: "Desktop", value: "desktop" },
  { label: "Phone", value: "mobile" },
];

/**
 * What the templates read in a preview: the person picked (or the sample one), the sender, the
 * campaign and the step. Snippets count as the AI's only while the step has instructions for it.
 */
const previewContext = ({
  campaign,
  person,
  position,
  sender,
  step,
}: {
  campaign: CampaignObject;
  person: PersonObject | null;
  position: number;
  sender: { email: string; name?: string | null };
  step: StepDraft;
}): PreviewContext => ({
  namespaces: {
    campaign: { id: campaign.id, name: campaign.name },
    person: person ?? SAMPLE_PERSON,
    sender: { email: sender.email, name: sender.name },
    step: { id: step.id, name: step.name, position },
    unsubscribe_url: "#",
  },
  sample: person === null,
  snippets: step.personalised && step.personalisation_prompt.trim() !== "",
});

/**
 * A variant as its recipient reads it, filled in for a person of the workspace or a sample one:
 * who it is from and to, the subject and preview text an inbox shows, and the body on a mail
 * client's canvas with the sender's signature, on a desktop's width or a phone's. A value the
 * person lacks is marked in red (the server would not create that email), an AI line by its name.
 * The sender is the one a test would be sent from by default (the campaign's pool, else the first
 * working mailbox).
 */
export const EmailPreview = ({
  campaign,
  className,
  fields,
  onPerson,
  person,
  position,
  step,
  variant,
}: {
  campaign: CampaignObject;
  className?: string | null;
  fields: readonly FieldName[];
  onPerson: (person: PersonObject | null) => void;
  person: PersonObject | null;
  position: number;
  step: StepDraft;
  variant: VariantDraft;
}) => {
  const workspace = useWorkspace();
  const senders = useQuery(sendersQuery(workspace));
  const [device, setDevice] = useState<Device>("desktop");
  const from = defaultSender(campaign, senders.data ?? [])?.identity;
  // Without a mailbox yet, the signed-in person stands in for the sender's details.
  const sender = from ?? workspace.session.me;
  const context = previewContext({ campaign, person, position, sender, step });
  const subject = renderTemplate(variant.subject, context);
  const preheader = variant.preheader
    ? renderTemplate(variant.preheader, context)
    : null;
  const body = renderTemplate(variant.body.html, context);
  const label = (path: string) => pathLabel(path, fields);
  const signatureSource = from ? signatureHtml(from) : null;
  const signature = signatureSource
    ? renderTemplate(signatureSource, context)
    : [];
  const unsupported = unsupportedTags([
    ...subject,
    ...(preheader ?? []),
    ...body,
    ...signature,
  ]);
  const missing = [
    ...new Set([
      ...missingPaths(subject),
      ...missingPaths(preheader ?? []),
      ...missingPaths(body),
      ...missingPaths(signature),
    ]),
  ].map(label);
  const recipient = person
    ? personLabel(person)
    : `${SAMPLE_PERSON.given_name} ${SAMPLE_PERSON.family_name} <${SAMPLE_PERSON.email}>`;
  return (
    <div className={cn("flex flex-col gap-3", className)}>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
        <label
          className="text-fg-3 text-sm"
          htmlFor={`${variant.key}-preview-person`}
        >
          Preview for
        </label>
        <div className="w-80 max-w-full min-w-0">
          <PersonPicker
            id={`${variant.key}-preview-person`}
            onChange={onPerson}
            placeholder={`${SAMPLE_PERSON.given_name} ${SAMPLE_PERSON.family_name} (sample), or search people`}
            value={person}
          />
        </div>
        <Segmented<Device>
          className="ml-auto"
          label="Screen"
          onChange={setDevice}
          options={DEVICES}
          value={device}
        />
      </div>
      <div
        className={cn(
          "border-line mx-auto w-full overflow-hidden rounded-lg border transition-[max-width] duration-200 ease-(--nb-ease-out) motion-reduce:transition-none",
          device === "mobile" ? "max-w-[390px]" : "max-w-full"
        )}
      >
        <dl className="border-line grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-1.5 border-b px-5 py-3 text-sm">
          <dt className="text-fg-3">From</dt>
          <dd className="text-fg-2 truncate">
            {from ? (
              formatAddress(from)
            ) : (
              <span className="text-fg-3">No mailbox connected yet</span>
            )}
          </dd>
          <dt className="text-fg-3">To</dt>
          <dd className="text-fg-2 truncate">{recipient}</dd>
          <dt className="text-fg-3">Subject</dt>
          <dd className="text-fg font-medium break-words">
            {variant.subject ? (
              <PieceText fields={fields} pieces={subject} />
            ) : (
              <span className="text-fg-3 font-normal">No subject yet</span>
            )}
          </dd>
          {preheader ? (
            <>
              <dt className="text-fg-3">Preview text</dt>
              <dd className="text-fg-2 break-words">
                <PieceText fields={fields} pieces={preheader} />
              </dd>
            </>
          ) : null}
        </dl>
        <MailFrame
          className="rounded-none border-0"
          html={`${piecesToHtml(body, label)}${signature.length > 0 ? `<br>${piecesToHtml(signature, label)}` : ""}`}
          readOnly
          title="Email preview"
        />
      </div>
      {unsupported.length > 0 ? (
        <p className="text-warning-fg text-xs" role="status">
          This preview cannot evaluate all template syntax. The remaining tags
          are not the final email. Inspect a prepared message under Messages to
          see its saved content.
        </p>
      ) : null}
      {missing.length > 0 ? (
        <p className="text-error-fg text-xs">
          No value for {missing.join(", ")}. An email that prints a missing
          value is not created: give each a fallback by clicking it in the
          email.
        </p>
      ) : (
        <p className="text-fg-3 text-xs">
          The unsubscribe link is added when the email is sent.
        </p>
      )}
    </div>
  );
};
