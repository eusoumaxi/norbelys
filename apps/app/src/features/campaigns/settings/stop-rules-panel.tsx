import type { StopOnReply } from "@norbelys/sdk";

import { SettingsPanel } from "@/components/settings-layout";
import { Input } from "@/components/ui/input";
import { Segmented } from "@/components/ui/segmented";
import type { PanelProps } from "@/features/campaigns/settings/schedule-panel";
import { FormField, SwitchField } from "@/lib/form";
import { problemAt } from "@/lib/problem";

const ON_REPLY_OPTIONS: { label: string; value: StopOnReply }[] = [
  { label: "This campaign", value: "campaign" },
  { label: "All their campaigns", value: "all" },
  { label: "Nothing", value: "none" },
];

const ON_REPLY_HELP: Record<StopOnReply, string> = {
  all: "Whoever replies gets no more emails from any campaign of this workspace.",
  campaign: "Whoever replies gets no more emails from this campaign.",
  none: "Replies change nothing: the sequence goes on.",
};

/**
 * When someone stops getting emails before the sequence ends: what their reply stops, whether
 * their colleagues stop too, and how long this campaign waits after another one wrote to them.
 */
export const StopRulesPanel = ({ draft, errors, set }: PanelProps) => (
  <SettingsPanel
    description="So nobody gets a follow-up after answering."
    title="When it stops"
  >
    <FormField
      description={ON_REPLY_HELP[draft.on_reply]}
      label="A reply stops"
      problem={problemAt(errors, "stop_rules.on_reply")}
    >
      <Segmented
        label="A reply stops"
        onChange={(value) => set({ on_reply: value })}
        options={ON_REPLY_OPTIONS}
        value={draft.on_reply}
      />
    </FormField>
    <SwitchField
      checked={draft.company_on_reply}
      description="When several people of one company are enrolled, a reply from one stops this campaign for all of them (same email domain)."
      id="stop-company"
      label="Stop their colleagues too"
      onChange={(checked) => set({ company_on_reply: checked })}
    />
    <FormField
      className="max-w-[280px]"
      description="Someone another campaign just wrote to waits this long before this one writes to them."
      htmlFor="stop-cooldown"
      label="Hours between campaigns"
      problem={problemAt(errors, "stop_rules.cooldown_hours")}
    >
      <Input
        aria-invalid={Boolean(problemAt(errors, "stop_rules.cooldown_hours"))}
        id="stop-cooldown"
        max={8760}
        min={0}
        onChange={(event) =>
          set({
            cooldown_hours: Math.max(
              0,
              Math.floor(Number(event.target.value) || 0)
            ),
          })
        }
        type="number"
        value={draft.cooldown_hours}
      />
    </FormField>
  </SettingsPanel>
);
