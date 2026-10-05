import { useQuery } from "@tanstack/react-query";

import { SettingsPanel } from "@/components/settings-layout";
import { Select } from "@/components/ui/select";
import type { SelectOption } from "@/components/ui/select";
import { trackingDomainsQuery } from "@/features/campaigns/queries";
import type { PanelProps } from "@/features/campaigns/settings/schedule-panel";
import { domainVerified } from "@/features/domains/queries";
import { FormField, SwitchField } from "@/lib/form";
import { humanize, shortId } from "@/lib/format";
import { problemAt } from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";

/** The platform's host, standing for `domain_id: null` in the choice. */
const PLATFORM = "platform";

/**
 * What the campaign's messages track: human opens, clicks, and the host their links go
 * through (the platform's, or a sending domain of the workspace with tracking turned on).
 */
export const TrackingPanel = ({ draft, errors, set }: PanelProps) => {
  const workspace = useWorkspace();
  const domains = useQuery(trackingDomainsQuery(workspace));
  const options: SelectOption[] = [
    { label: "Norbelys (default)", value: PLATFORM },
    ...(domains.data ?? [])
      .filter((d) => d.tracking_enabled || d.id === draft.domain_id)
      .map((d) => ({
        label: domainVerified(d)
          ? d.hostname
          : `${d.hostname} (${humanize(d.status).toLowerCase()})`,
        value: d.id,
      })),
  ];
  if (draft.domain_id && !options.some((o) => o.value === draft.domain_id)) {
    options.push({ label: shortId(draft.domain_id), value: draft.domain_id });
  }
  return (
    <SettingsPanel
      description="Opens and clicks are counted for people only: scanners and privacy proxies are left out of the figures."
      title="Tracking"
    >
      <SwitchField
        checked={draft.opens}
        description="An invisible image in each message. Many inboxes load images by themselves, so opens are a weak signal."
        id="tracking-opens"
        label="Track opens"
        onChange={(checked) => set({ opens: checked })}
      />
      <SwitchField
        checked={draft.clicks}
        description="Each link passes through a tracking address on its way to the page."
        id="tracking-clicks"
        label="Track clicks"
        onChange={(checked) => set({ clicks: checked })}
      />
      <FormField
        className="max-w-[400px]"
        description="Your own sending domain, once tracking is turned on for it, makes tracked links look like yours."
        htmlFor="tracking-domain"
        label="Tracked links use"
        problem={
          problemAt(errors, "tracking.domain_id") ??
          problemAt(errors, "tracking")
        }
      >
        <Select
          id="tracking-domain"
          onChange={(value) =>
            set({ domain_id: value === PLATFORM ? "" : value })
          }
          options={options}
          value={draft.domain_id || PLATFORM}
        />
      </FormField>
    </SettingsPanel>
  );
};
