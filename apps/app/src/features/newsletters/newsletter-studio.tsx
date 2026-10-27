import type { CampaignObject } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState } from "react";

import { PageBody, PageHeader } from "@/components/page";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { campaignsKey, sendersQuery } from "@/features/campaigns/queries";
import { MailFrame } from "@/features/messages/body-editor";
import { NewsletterEngagement } from "@/features/newsletters/engagement";
import {
  NEWSLETTER_PREFIX,
  readPack,
  loadNewsletters,
  prepareDrafts,
} from "@/features/newsletters/model";
import type { NewsletterPack } from "@/features/newsletters/model";
import { canWrite, useWorkspace } from "@/lib/workspace";

const SavedEdition = ({ campaign }: { campaign: CampaignObject }) => {
  const workspace = useWorkspace();
  const { stats } = campaign;
  const metrics = [
    ["Sent", stats.sent],
    ["Open recorded", stats.opened],
    ["Delivered", stats.delivered],
    ["Click recorded", stats.clicked],
    ["Unsubscribed", stats.unsubscribed],
  ] as const;
  return (
    <article className="border-line rounded-lg border p-5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <p className="text-fg-3 text-xs tracking-wide uppercase">
            {campaign.status}
          </p>
          <h3 className="mt-1 font-semibold">
            {campaign.name.replace(NEWSLETTER_PREFIX, "")}
          </h3>
          <p className="text-fg-2 mt-1 text-sm">{campaign.steps[0]?.name}</p>
        </div>
        <Link
          className="text-accent text-sm underline"
          params={{ slug: workspace.slug, campaignId: campaign.id }}
          to="/w/$slug/campaigns/$campaignId"
        >
          Review content, audience and delivery
        </Link>
      </div>
      <dl className="mt-5 grid grid-cols-2 gap-4 sm:grid-cols-5">
        {metrics.map(([label, value]) => (
          <div key={label}>
            <dt className="text-fg-3 text-xs">{label}</dt>
            <dd className="mt-1 text-xl tabular-nums">
              {stats.computed_at ? value.toLocaleString() : "—"}
            </dd>
          </div>
        ))}
      </dl>
      <p className="text-fg-3 mt-4 text-xs">
        {campaign.schedule.start_at
          ? `Proposed start: ${new Date(campaign.schedule.start_at).toLocaleString("en", { timeZone: campaign.schedule.timezone })} (${campaign.schedule.timezone}). `
          : "No date selected. "}
        {campaign.status === "draft"
          ? "Draft: this date cannot start delivery. Review and start separately."
          : "Delivery follows the sender's limits."}
      </p>
    </article>
  );
};

/** A newsletter is a one-step campaign, retaining the server's queue, suppression and tracking. */
export const NewsletterStudio = () => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const [pack, setPack] = useState<NewsletterPack | null>(null);
  const [selected, setSelected] = useState(0);
  const [senderId, setSenderId] = useState("");
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState(0);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const senders = useQuery(sendersQuery(workspace));
  const saved = useQuery({
    queryKey: [...campaignsKey(workspace), "newsletters"],
    queryFn: ({ signal }) => loadNewsletters(workspace.api, signal),
    refetchInterval: 60_000,
  });
  const sender = senders.data?.find((entry) => entry.identity.id === senderId);
  const writable = canWrite(workspace);
  const importPack = (file: File) => {
    setError("");
    setNotice("");
    void readPack(file)
      .then((imported) => {
        setPack(imported);
        setSelected(0);
        setProgress(0);
      })
      .catch((error: unknown) => {
        setError(
          error instanceof Error ? error.message : "Could not import this pack."
        );
      });
  };
  const prepare = () => {
    if (!pack || !sender || !writable || busy) {
      return;
    }
    setBusy(true);
    setError("");
    setNotice("");
    void prepareDrafts(workspace.api, workspace.id, pack, senderId, setProgress)
      .then(() =>
        setNotice(
          "Drafts saved. No audience was enrolled and no newsletter was started. Review each audience before activation."
        )
      )
      .catch((error: unknown) =>
        setError(
          error instanceof Error
            ? error.message
            : "Could not save every draft. Retry to resume."
        )
      )
      .finally(() => {
        setBusy(false);
        void queryClient.invalidateQueries({
          queryKey: campaignsKey(workspace),
        });
      });
  };
  return (
    <>
      <PageHeader
        title="Newsletters"
        subtitle="One edition. One audience. One broadcast."
      />
      <PageBody>
        <div className="mx-auto flex w-full max-w-6xl flex-col gap-8 py-6">
          <section className="border-line rounded-xl border p-6">
            <p className="text-accent text-xs font-semibold tracking-widest uppercase">
              Prepare · Review · Send
            </p>
            <h2 className="mt-3 text-2xl font-semibold">
              A quiet place to prepare your next edition
            </h2>
            <p className="text-fg-2 mt-2 max-w-2xl text-sm">
              Import a content pack, preview every edition and save independent
              drafts. Dates are proposals until you explicitly start an edition.
              Opens and clicks use the existing campaign tracking.
            </p>
            <div className="mt-5 flex flex-wrap items-end gap-5">
              <label
                className="flex flex-col gap-2 text-sm"
                htmlFor="newsletter-pack"
              >
                Content pack (JSON)
                <Input
                  accept="application/json,.json"
                  disabled={!writable || busy}
                  id="newsletter-pack"
                  onChange={(event) => {
                    const file = event.target.files?.[0];
                    if (file) {
                      void importPack(file);
                    }
                  }}
                  type="file"
                />
              </label>
              <label
                className="flex min-w-64 flex-col gap-2 text-sm"
                htmlFor="newsletter-sender"
              >
                Sender
                <select
                  className="border-line bg-surface rounded-md border p-2"
                  disabled={!writable || busy || senders.isPending}
                  id="newsletter-sender"
                  onChange={(event) => setSenderId(event.target.value)}
                  value={senderId}
                >
                  <option value="">Choose a connected sender</option>
                  {senders.data
                    ?.filter(
                      (entry) =>
                        entry.identity.enabled &&
                        entry.connection.status === "active" &&
                        !entry.connection.paused
                    )
                    .map((entry) => (
                      <option key={entry.identity.id} value={entry.identity.id}>
                        {entry.identity.email} · {entry.connection.provider}
                      </option>
                    ))}
                </select>
              </label>
              <Button
                disabled={!pack || !sender || busy || !writable}
                onClick={() => {
                  void prepare();
                }}
                variant="primary"
              >
                {busy
                  ? `Saving ${progress}/${pack?.editions.length ?? 0}`
                  : `Save ${pack?.editions.length ?? "all"} as drafts`}
              </Button>
            </div>
            {sender ? (
              <p className="text-fg-2 mt-3 text-sm">
                Sender limit: {sender.connection.daily_limit.toLocaleString()}{" "}
                messages/day.
                {sender.connection.send_interval_minutes
                  ? ` Paced at one message every ${sender.connection.send_interval_minutes} minutes.`
                  : " Rate-paced queue."}{" "}
                Larger audiences wait for quota; importing never changes these
                limits.
              </p>
            ) : null}
            {error ? (
              <p className="mt-4 text-sm" role="alert">
                {error}
              </p>
            ) : null}
            {senders.isError || saved.isError ? (
              <p className="mt-4 text-sm" role="alert">
                Could not load workspace data. Refresh to retry.
              </p>
            ) : null}
            {notice ? (
              <output className="text-accent mt-4 block text-sm">
                {notice}
              </output>
            ) : null}
          </section>
          <PackPreview pack={pack} selected={selected} onSelect={setSelected} />
          <SavedEditions campaigns={saved.data} loading={saved.isPending} />
          <NewsletterEngagement campaigns={saved.data ?? []} />
          <p className="text-fg-3 max-w-3xl text-xs">
            An open is an observed signal, not proof of reading. Privacy proxies
            and blocked images affect it. “No open recorded” is not an unread
            count. Editorial categories do not control Gmail tabs. Use consented
            audiences; unsubscribes and suppressions remain enforced by the
            server.
          </p>
        </div>
      </PageBody>
    </>
  );
};

const SavedEditions = ({
  campaigns,
  loading,
}: {
  campaigns: CampaignObject[] | undefined;
  loading: boolean;
}) => (
  <section className="flex flex-col gap-4">
    <h2 className="text-lg font-semibold">Saved editions</h2>
    {loading ? <p className="text-fg-2 text-sm">Loading editions…</p> : null}
    {campaigns?.length === 0 ? (
      <p className="text-fg-2 text-sm">
        No newsletters saved yet. Import a pack to prepare the first draft.
      </p>
    ) : null}
    {campaigns?.map((campaign) => (
      <SavedEdition campaign={campaign} key={campaign.id} />
    ))}
  </section>
);

const PackPreview = ({
  pack,
  selected,
  onSelect,
}: {
  pack: NewsletterPack | null;
  selected: number;
  onSelect: (index: number) => void;
}) => {
  const edition = pack?.editions[selected];
  return (
    <>
      {pack && edition ? (
        <section className="grid gap-6 lg:grid-cols-[280px_1fr]">
          <div>
            <h2 className="mb-3 font-semibold">{pack.name}</h2>
            <div className="flex flex-col gap-2">
              {pack.editions.map((item, index) => (
                <button
                  aria-pressed={index === selected}
                  className="border-line hover:bg-hover aria-pressed:bg-selected rounded-lg border p-4 text-left"
                  key={item.id}
                  onClick={() => onSelect(index)}
                  type="button"
                >
                  <span className="text-fg-3 text-xs">
                    {String(index + 1).padStart(2, "0")} · {item.category}
                  </span>
                  <span className="mt-1 block text-sm font-medium">
                    {item.title}
                  </span>
                </button>
              ))}
            </div>
          </div>
          <div className="min-w-0">
            <div className="border-line mb-4 rounded-lg border p-5">
              <p className="text-fg-3 text-xs uppercase">Preview · Not sent</p>
              <h2 className="mt-2 text-xl font-semibold">{edition.subject}</h2>
              <p className="text-fg-2 mt-2 text-sm">{edition.preheader}</p>
              <p className="mt-4 text-sm">
                <strong>Intended audience:</strong> {edition.audience}
              </p>
              <p className="text-fg-2 mt-2 text-sm">
                Select the matching consented group or segment in the saved
                draft. This description does not enroll contacts.
              </p>
              <a
                className="text-accent mt-3 inline-block text-sm underline"
                href={edition.source}
                rel="noreferrer"
                target="_blank"
              >
                Editorial source
              </a>
            </div>
            <MailFrame
              html={edition.html}
              readOnly
              title={`${edition.title} newsletter preview`}
            />
          </div>
        </section>
      ) : null}
    </>
  );
};
