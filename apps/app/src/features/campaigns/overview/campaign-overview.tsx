import { Illustration } from "@/components/illustration";
import { CampaignActivity } from "@/features/campaigns/overview/campaign-activity";
import { CampaignJourney } from "@/features/campaigns/overview/campaign-journey";
import { CampaignMetrics } from "@/features/campaigns/overview/campaign-metrics";
import { CampaignResults } from "@/features/campaigns/overview/campaign-results";
import { enrollmentSummary, useCampaign } from "@/features/campaigns/queries";

/** What the results section says before there is anything to count. */
const NOT_YET: Record<string, string> = {
  active:
    "The first figures arrive a few minutes after the first emails go out: replies, bounces and, when tracked, opens and clicks, step by step.",
  draft:
    "Once it starts, replies, bounces and, when tracked, opens and clicks show up here, step by step.",
};

/**
 * A campaign's front tab, read top to bottom as a person asks about it: where everyone is in the
 * sequence (and how it sends), then what came of it. Results appear once something was sent and
 * counted; before that, one sentence says when they will.
 */
export const CampaignOverview = () => {
  const campaign = useCampaign();
  const reported =
    Boolean(campaign.stats.computed_at) && campaign.stats.sent > 0;
  return (
    <div className="flex flex-col gap-10">
      <CampaignJourney
        campaign={campaign}
        summary={enrollmentSummary(campaign)}
      />
      <section className="flex flex-col gap-4">
        <h2 className="text-fg text-xl font-semibold">Results</h2>
        {reported ? (
          <>
            <CampaignMetrics campaign={campaign} />
            <CampaignActivity campaign={campaign} />
            <CampaignResults campaign={campaign} />
          </>
        ) : (
          <div className="border-line flex items-center gap-6 rounded-sm border px-6 py-4">
            <Illustration
              className="hidden w-[132px] sm:block"
              name="reports"
              once="nb.drawn.reports"
            />
            <div className="flex flex-col gap-1">
              <p className="text-fg text-base font-medium">
                Nothing to count yet
              </p>
              <p className="text-fg-3 max-w-[560px] text-sm">
                {NOT_YET[campaign.status] ?? NOT_YET.active}
              </p>
            </div>
          </div>
        )}
      </section>
    </div>
  );
};
