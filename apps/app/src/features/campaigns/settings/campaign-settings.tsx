import type { CampaignObject, UpdateCampaign } from "@norbelys/sdk";

import {
  ArchivedNote,
  SaveBar,
  SaveProblem,
  useCampaignEditor,
} from "@/features/campaigns/campaign-editor";
import { campaignEditable, useCampaign } from "@/features/campaigns/queries";
import { SchedulePanel } from "@/features/campaigns/settings/schedule-panel";
import { SendersPanel } from "@/features/campaigns/settings/senders-panel";
import {
  settingsOf,
  settingsUpdate,
  unplacedSettings,
} from "@/features/campaigns/settings/settings-draft";
import type { SettingsDraft } from "@/features/campaigns/settings/settings-draft";
import { StopRulesPanel } from "@/features/campaigns/settings/stop-rules-panel";
import { TrackingPanel } from "@/features/campaigns/settings/tracking-panel";
import { fieldProblems } from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";

/** The settings that differ from the campaign's, when any does. */
const settingsChanges = (
  draft: SettingsDraft,
  base: CampaignObject
): UpdateCampaign | null => {
  const update = settingsUpdate(draft, base);
  return Object.keys(update).length > 0 ? update : null;
};

/**
 * A campaign's settings as one form, in two columns on a wide screen so it reads at a glance:
 * when it sends and when it stops, then who sends it and what it tracks. Save sends only
 * what changed to `campaigns.update`, checked against the version that was opened, and the API's
 * errors show beside the inputs they name.
 */
export const CampaignSettings = () => {
  const campaign = useCampaign();
  const workspace = useWorkspace();
  const editor = useCampaignEditor(
    campaign,
    settingsOf,
    settingsChanges,
    "Settings saved"
  );
  const readOnly = !campaignEditable(workspace, campaign);
  const errors = fieldProblems(editor.failure);
  const props = {
    draft: editor.draft,
    errors,
    set: (patch: Partial<SettingsDraft>) =>
      editor.setDraft((current) => ({ ...current, ...patch })),
  };

  return (
    <div className="flex max-w-[1240px] flex-col gap-6">
      {/* The save lives with the work, in a row that stays at the top while the form scrolls. */}
      {readOnly ? null : (
        <div className="bg-surface border-line sticky top-0 z-10 -mt-2 flex min-h-12 flex-wrap items-center justify-between gap-x-4 gap-y-2 border-b py-2">
          <p className="text-fg-3 text-sm">
            Changes apply to the emails that have not gone out yet.
          </p>
          <SaveBar
            dirty={editor.dirty}
            divided={false}
            label="Save settings"
            onDiscard={() => editor.reset(editor.base)}
            onSave={() => {
              void editor.save();
            }}
            saving={editor.saving}
          />
        </div>
      )}
      <div className="flex flex-col gap-9">
        <ArchivedNote campaign={campaign} />
        <SaveProblem
          failure={editor.failure}
          onReload={editor.handleReload}
          unplaced={unplacedSettings(errors)}
        />
        <fieldset
          className="grid min-w-0 items-start gap-x-14 gap-y-9 xl:grid-cols-2"
          disabled={readOnly}
        >
          <legend className="sr-only">Campaign settings</legend>
          <div className="flex min-w-0 flex-col gap-9">
            <SchedulePanel {...props} />
            <StopRulesPanel {...props} />
          </div>
          <div className="flex min-w-0 flex-col gap-9">
            <SendersPanel {...props} />
            <TrackingPanel {...props} />
          </div>
        </fieldset>
      </div>
    </div>
  );
};
