import { Alert02Icon, Tick02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { APIError } from "@norbelys/sdk";
import type { CampaignObject, UpdateCampaign } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useId, useState, useSyncExternalStore } from "react";
import { toast } from "sonner";

import { SaveFailure } from "@/components/problem";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { campaignKey, campaignsKey } from "@/features/campaigns/queries";
import { useWorkspace } from "@/lib/workspace";

/** The editors holding unsaved changes, by the campaign they edit. */
const unsaved = new Map<string, Set<string>>();
const listeners = new Set<() => void>();

const subscribe = (listener: () => void) => {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
};

/** Notes that `editor` of `campaign` holds unsaved changes (or no longer does). */
const mark = (campaign: string, editor: string, dirty: boolean) => {
  const editors = unsaved.get(campaign) ?? new Set<string>();
  if (dirty) {
    editors.add(editor);
  } else {
    editors.delete(editor);
  }
  unsaved.set(campaign, editors);
  for (const listener of listeners) {
    listener();
  }
};

/**
 * Whether one of the campaign's editors (its sequence, its settings) holds changes not saved
 * yet: what an action on the whole campaign, such as starting it, checks first.
 */
export const useUnsavedChanges = (campaignId: string): boolean =>
  useSyncExternalStore(
    subscribe,
    () => (unsaved.get(campaignId)?.size ?? 0) > 0
  );

/**
 * The state of an editor of part of a campaign (its sequence, its settings): the campaign as it
 * was opened (its version is the save's `If-Match`), the draft `draftOf` reads from it, and the
 * last refused save. While nothing is edited the draft follows the campaign as the API changes it
 * (a winner chosen, a status moved); once something is, it keeps the opened version, so a save
 * never overwrites a change it did not see. `changes` is the update that turns the opened
 * campaign into the draft, `null` while there is none.
 */
export const useCampaignEditor = <D,>(
  campaign: CampaignObject,
  draftOf: (campaign: CampaignObject) => D,
  changes: (draft: D, base: CampaignObject) => UpdateCampaign | null,
  savedLabel: string
) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const [base, setBase] = useState(campaign);
  const [draft, setDraft] = useState(() => draftOf(campaign));
  const [failure, setFailure] = useState<unknown>(null);
  const [saving, setSaving] = useState(false);
  const update = changes(draft, base);
  const dirty = update !== null;
  const editor = useId();
  useEffect(() => {
    if (!dirty) {
      return;
    }
    mark(campaign.id, editor, true);
    return () => mark(campaign.id, editor, false);
  }, [campaign.id, dirty, editor]);
  if (campaign.version !== base.version && update === null) {
    setBase(campaign);
    setDraft(draftOf(campaign));
  }
  const reset = (to: CampaignObject) => {
    setBase(to);
    setDraft(draftOf(to));
    setFailure(null);
  };
  const save = async () => {
    if (!update) {
      return;
    }
    setSaving(true);
    setFailure(null);
    try {
      const updated = await workspace.api.campaigns.update(
        campaign.id,
        update,
        { headers: { "If-Match": `"${base.version}"` } }
      );
      queryClient.setQueryData(campaignKey(workspace, campaign.id), updated);
      reset(updated);
      toast.success(savedLabel);
      void queryClient.invalidateQueries({
        queryKey: [...campaignsKey(workspace), "list"],
      });
    } catch (error) {
      setFailure(error);
    }
    setSaving(false);
  };
  /** Throws the edits away and reads the campaign again, after a save refused for its version. */
  const handleReload = () => {
    void queryClient.invalidateQueries({
      queryKey: campaignKey(workspace, campaign.id),
    });
    reset(campaign);
  };
  return {
    base,
    dirty,
    draft,
    failure,
    handleReload,
    reset,
    save,
    saving,
    setDraft,
    setFailure,
  };
};

/** The line that says an archived campaign is read-only, above its editors. */
export const ArchivedNote = ({ campaign }: { campaign: CampaignObject }) =>
  campaign.status === "archived" ? (
    <p className="text-fg-2 text-sm">An archived campaign cannot be changed.</p>
  ) : null;

/**
 * The save state of an editor, at the end of its top row beside the buttons it follows (a hairline
 * divides them unless `divided` is off): a quiet "Saved" while nothing is pending; "Discard" and
 * "Save" once something is, with a spinner while it saves and, with `note`, what saving does on
 * hover. The states cross-fade. ⌘S or Ctrl+S saves from anywhere on the page, and leaving the
 * page with unsaved changes asks first. Read-only editors show none of it.
 */
export const SaveBar = ({
  dirty,
  divided = true,
  label,
  note,
  onDiscard,
  onSave,
  saving,
}: {
  dirty: boolean;
  divided?: boolean;
  /** What saving saves, for assistive technology: "Save sequence". */
  label: string;
  note?: string;
  onDiscard: () => void;
  onSave: () => void;
  saving: boolean;
}) => {
  useEffect(() => {
    if (!dirty) {
      return;
    }
    const save = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "s") {
        event.preventDefault();
        if (!saving) {
          onSave();
        }
      }
    };
    const leave = (event: BeforeUnloadEvent) => event.preventDefault();
    window.addEventListener("keydown", save);
    window.addEventListener("beforeunload", leave);
    return () => {
      window.removeEventListener("keydown", save);
      window.removeEventListener("beforeunload", leave);
    };
  }, [dirty, onSave, saving]);
  const fade =
    "flex items-center gap-2 transition-opacity duration-150 starting:opacity-0 motion-reduce:transition-none";
  const save = (
    <Button
      aria-keyshortcuts="Meta+S Control+S"
      aria-label={label}
      disabled={saving}
      onClick={onSave}
      variant="primary"
    >
      {saving ? <Spinner /> : null}
      Save
    </Button>
  );
  return (
    <div className="flex h-8 shrink-0 items-center gap-2">
      {divided ? (
        <span aria-hidden className="bg-line mr-1 h-5 w-px max-sm:hidden" />
      ) : null}
      {dirty ? (
        <div className={fade} key="unsaved">
          <Button disabled={saving} onClick={onDiscard} variant="tertiary">
            Discard
          </Button>
          {note ? (
            <Tooltip>
              <TooltipTrigger render={save} />
              <TooltipContent>{note}</TooltipContent>
            </Tooltip>
          ) : (
            save
          )}
        </div>
      ) : (
        <p className={`text-fg-3 text-sm ${fade} gap-1.5`} key="saved">
          <HugeiconsIcon className="size-3.5" icon={Tick02Icon} />
          Saved
        </p>
      )}
    </div>
  );
};

/**
 * Why the last save was refused, above the editor: someone else changed the campaign since it
 * was opened (`412`: nothing was written; load the latest), or what the API said, with the errors
 * no input shows. Errors that belong to an input show beside it instead.
 */
export const SaveProblem = ({
  failure,
  onReload,
  unplaced,
}: {
  failure: unknown;
  onReload: () => void;
  unplaced: [string, string][];
}) => {
  if (failure instanceof APIError && failure.status === 412) {
    return (
      <Alert variant="warning">
        <HugeiconsIcon icon={Alert02Icon} />
        <AlertTitle>The campaign changed since you opened it</AlertTitle>
        <AlertDescription className="flex flex-col items-start gap-3">
          Nothing was saved, so no one&apos;s change was lost. Load the latest
          version to edit it (your unsaved changes here are discarded).
          <Button onClick={onReload} size="s">
            Load the latest
          </Button>
        </AlertDescription>
      </Alert>
    );
  }
  return <SaveFailure failure={failure} unplaced={unplaced} />;
};
