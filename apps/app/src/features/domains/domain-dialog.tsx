import type { DomainObject, DomainPurpose } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";

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
import { Input } from "@/components/ui/input";
import { Select } from "@/components/ui/select";
import { domainQuery, domainsKey } from "@/features/domains/queries";
import { FormField, SwitchField } from "@/lib/form";
import { fieldProblems, problemLine } from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";

export const DOMAIN_USES: Record<DomainPurpose, string> = {
  receive: "Receive email",
  send: "Send email",
  send_receive: "Send and receive email",
  tracking: "Tracking links only",
};

/** Unknown or empty selections stay unselected until the person chooses a use. */
const purposeValue = (value: string): DomainPurpose | null => {
  switch (value) {
    case "receive":
    case "send":
    case "send_receive":
    case "tracking":
      return value;
    default:
      return null;
  }
};

/** The same explicit domain choices at creation and editing, with no DNS writes. */
export const DomainDialog = ({
  domain,
  onOpenChange,
  onSaved,
  open,
}: {
  domain?: DomainObject;
  onOpenChange: (open: boolean) => void;
  onSaved?: (domain: DomainObject) => void;
  open: boolean;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const [hostname, setHostname] = useState(domain?.hostname ?? "");
  const [purpose, setPurpose] = useState<DomainPurpose | null>(
    domain?.purpose ?? null
  );
  const [customTracking, setCustomTracking] = useState(
    Boolean(domain?.tracking_domain)
  );
  const [trackingHostname, setTrackingHostname] = useState(
    domain?.tracking_domain?.hostname ?? ""
  );
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const problems = fieldProblems(failure);
  const separateTracking =
    purpose !== null && purpose !== "tracking" && customTracking;
  const ready =
    hostname.trim() !== "" &&
    purpose !== null &&
    (!separateTracking || trackingHostname.trim() !== "");
  const receiving = purpose === "receive" || purpose === "send_receive";

  return (
    <Dialog onOpenChange={onOpenChange} open={open}>
      <DialogContent>
        <form
          className="flex min-h-0 flex-col"
          onSubmit={async (event) => {
            event.preventDefault();
            if (!ready || purpose === null || busy) {
              return;
            }
            setBusy(true);
            setFailure(null);
            try {
              const saved = domain
                ? await workspace.api.sendingDomains.update(
                    domain.id,
                    {
                      purpose,
                      tracking_hostname: separateTracking
                        ? trackingHostname.trim()
                        : null,
                    },
                    { ifMatch: domain.version }
                  )
                : await workspace.api.sendingDomains.create({
                    hostname: hostname.trim(),
                    purpose,
                    ...(separateTracking
                      ? { tracking_hostname: trackingHostname.trim() }
                      : {}),
                  });
              queryClient.setQueryData(
                domainQuery(workspace, saved.id).queryKey,
                saved
              );
              await queryClient.invalidateQueries({
                queryKey: domainsKey(workspace),
              });
              onSaved?.(saved);
              onOpenChange(false);
            } catch (error) {
              setFailure(error);
            }
            setBusy(false);
          }}
        >
          <DialogHeader>
            <DialogTitle>{domain ? "Edit domain" : "Add domain"}</DialogTitle>
            <DialogDescription>
              Choose how to use your domain. We prepare the DNS records for your
              choices; you publish them with your DNS provider.
            </DialogDescription>
          </DialogHeader>
          <DialogBody>
            <FormField
              htmlFor="domain-hostname"
              label="Domain"
              problem={problems.hostname}
            >
              <Input
                autoFocus
                className="font-mono"
                disabled={Boolean(domain) || busy}
                id="domain-hostname"
                onChange={(event) => setHostname(event.target.value)}
                placeholder="example.com"
                required
                value={hostname}
              />
            </FormField>
            <FormField
              htmlFor="domain-purpose"
              label="Use this domain for"
              problem={problems.purpose}
            >
              <Select
                disabled={busy}
                id="domain-purpose"
                onChange={(value) => setPurpose(purposeValue(value))}
                options={Object.entries(DOMAIN_USES).map(([value, label]) => ({
                  label,
                  value,
                }))}
                placeholder="Choose a use"
                value={purpose}
              />
            </FormField>
            {purpose !== null && purpose !== "tracking" ? (
              <SwitchField
                checked={customTracking}
                description="Optional: use another hostname for custom open and click tracking."
                id="domain-tracking"
                label="Add custom tracking links"
                onChange={setCustomTracking}
              />
            ) : null}
            {separateTracking ? (
              <FormField
                description="Choose a separate hostname. A tracking CNAME cannot share its name with mail or website records."
                htmlFor="domain-tracking-hostname"
                label="Tracking hostname"
                problem={problems.tracking_hostname}
              >
                <Input
                  className="font-mono"
                  disabled={busy}
                  id="domain-tracking-hostname"
                  onChange={(event) => setTrackingHostname(event.target.value)}
                  placeholder={`links.${hostname.trim() || "example.com"}`}
                  required
                  value={trackingHostname}
                />
              </FormField>
            ) : null}
            {receiving ? (
              <p className="text-fg-2 text-sm">
                Receiving through Norbelys requires MX records and a mailbox. If
                you already use Google Workspace or Microsoft 365, keep their MX
                to continue receiving there, or choose a separate receiving
                subdomain.
              </p>
            ) : null}
            {purpose === "send" ? (
              <p className="text-fg-2 text-sm">
                Keep your existing MX records. Sending through Norbelys adds
                authentication records without moving incoming email.
              </p>
            ) : null}
            {purpose === "tracking" ? (
              <p className="text-fg-2 text-sm">
                This hostname serves links through a CNAME. Choose a name
                without existing mail or website records.
              </p>
            ) : null}
            {domain ? (
              <p className="text-fg-3 text-xs">
                Disabling a mail direction stops that operation and keeps its
                history. Enabling sending again leaves sender identities
                disabled until you enable them in Mailboxes.
              </p>
            ) : null}
            {failure ? (
              <ProblemAlert>{problemLine(failure)}</ProblemAlert>
            ) : null}
          </DialogBody>
          <DialogActions>
            <SubmitButton busy={busy} disabled={!ready}>
              {domain ? "Save changes" : "Add domain"}
            </SubmitButton>
          </DialogActions>
        </form>
      </DialogContent>
    </Dialog>
  );
};
