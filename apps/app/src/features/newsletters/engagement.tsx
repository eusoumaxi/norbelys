import type { CampaignObject, Meta } from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";
import { useState } from "react";

import { Button } from "@/components/ui/button";
import { useWorkspace } from "@/lib/workspace";

interface Recipient {
  person_id: string;
  email: string;
  sent: number;
  opened: number;
  clicked: number;
  opens_tracked: boolean;
}

interface Recipients {
  data: Recipient[];
  meta: Meta;
}

const OPTIONS = [
  ["all", "All sent recipients"],
  ["opened", "Open recorded"],
  ["not_opened", "No open or click recorded"],
  ["clicked", "Click recorded"],
] as const;

/** CSV cells cannot become spreadsheet formulas when exported addresses start with punctuation. */
export const csvCell = (value: string): string => {
  const safe = /^[=+@\-\t\r\n]/u.test(value) ? `'${value}` : value;
  return `"${safe.replaceAll('"', '""')}"`;
};

const downloadAudience = async (
  read: (after: string | null) => Promise<Recipients>,
  filename: string
): Promise<void> => {
  const rows = ["email,person_id,sent,opened,clicked"];
  const seen = new Set<string>();
  let after: string | null = null;
  do {
    // oxlint-disable-next-line no-await-in-loop -- cursor pages must be fetched in order
    const page = await read(after);
    for (const person of page.data) {
      if (!seen.has(person.person_id)) {
        seen.add(person.person_id);
        rows.push(
          [
            csvCell(person.email),
            csvCell(person.person_id),
            person.sent,
            person.opened,
            person.clicked,
          ].join(",")
        );
      }
    }
    after = page.meta.has_more ? (page.meta.next_cursor ?? null) : null;
    if (page.meta.has_more && !after) {
      throw new Error(
        "The server returned an incomplete page. No file was exported."
      );
    }
    if (seen.size >= 20_000 && after) {
      throw new Error(
        "This export exceeds 20,000 people. Narrow the audience before exporting."
      );
    }
  } while (after);
  const url = URL.createObjectURL(
    new Blob([rows.join("\r\n")], { type: "text/csv;charset=utf-8" })
  );
  const link = document.createElement("a");
  link.href = url;
  link.download = filename;
  link.click();
  URL.revokeObjectURL(url);
};

export const NewsletterEngagement = ({
  campaigns,
}: {
  campaigns: CampaignObject[];
}) => {
  const workspace = useWorkspace();
  const [campaignId, setCampaignId] = useState("");
  const [engagement, setEngagement] = useState("all");
  const [cursor, setCursor] = useState<string | null>(null);
  const [exporting, setExporting] = useState(false);
  const [failure, setFailure] = useState("");
  const read = (
    after: string | null,
    signal?: AbortSignal
  ): Promise<Recipients> => {
    const params = new URLSearchParams({
      engagement,
      limit: "100",
      order: "asc",
    });
    if (after) {
      params.set("cursor", after);
    }
    return workspace.session.workspace(
      workspace.id,
      "GET",
      `/newsletters/${encodeURIComponent(campaignId)}/recipients?${params.toString()}`,
      { signal }
    );
  };
  const query = useQuery({
    queryKey: [
      workspace.id,
      "newsletter-recipients",
      campaignId,
      engagement,
      cursor,
    ],
    queryFn: ({ signal }) => read(cursor, signal),
    enabled: Boolean(campaignId),
  });
  const exportAudience = async () => {
    setExporting(true);
    setFailure("");
    try {
      await downloadAudience(
        read,
        `newsletter-${campaignId}-${engagement}.csv`
      );
    } catch (error) {
      setFailure(error instanceof Error ? error.message : "The export failed.");
    }
    setExporting(false);
  };
  return (
    <section className="border-line rounded-lg border p-5">
      <h2 className="text-lg font-semibold">Engagement audiences</h2>
      <p className="text-fg-2 mt-2 max-w-3xl text-sm">
        Explore sent recipients in the last 30 days of online history. Open and
        click filters use signals classified as human. The negative filter
        excludes every recorded open or click, including proxies, and requires
        open tracking. It does not mean unread.
      </p>
      <div className="mt-4 flex flex-wrap items-end gap-4">
        <label
          className="flex flex-col gap-2 text-sm"
          htmlFor="engagement-edition"
        >
          Source edition
          <select
            className="border-line rounded border p-2"
            disabled={exporting}
            id="engagement-edition"
            onChange={(event) => {
              setCampaignId(event.target.value);
              setCursor(null);
            }}
            value={campaignId}
          >
            <option value="">Choose an edition</option>
            {campaigns.map((campaign) => (
              <option key={campaign.id} value={campaign.id}>
                {campaign.name}
              </option>
            ))}
          </select>
        </label>
        <label
          className="flex flex-col gap-2 text-sm"
          htmlFor="engagement-filter"
        >
          Recorded activity
          <select
            className="border-line rounded border p-2"
            disabled={exporting}
            id="engagement-filter"
            onChange={(event) => {
              setEngagement(event.target.value);
              setCursor(null);
            }}
            value={engagement}
          >
            {OPTIONS.map(([value, label]) => (
              <option key={value} value={value}>
                {label}
              </option>
            ))}
          </select>
        </label>
        <Button
          disabled={!campaignId || exporting || query.isError}
          onClick={() => {
            void exportAudience();
          }}
        >
          {exporting ? "Exporting…" : "Export audience CSV"}
        </Button>
      </div>
      <p className="text-fg-3 mt-3 text-xs">
        Export reads all pages, up to 20,000 people, without sending. For a new
        audience, import the email column into a dedicated group, intersect it
        with your consent and profile criteria, then review the draft. Activity
        can change while pages are read.
      </p>
      {query.isError || failure ? (
        <p className="mt-3 text-sm" role="alert">
          {failure ||
            "Could not load this report. The server must include the newsletter dashboard update."}
        </p>
      ) : null}
      {query.data ? (
        <>
          <div className="mt-4 overflow-auto">
            <table className="w-full text-left text-sm">
              <thead>
                <tr>
                  <th className="p-2">Recipient</th>
                  <th className="p-2">Sent</th>
                  <th className="p-2">Open recorded</th>
                  <th className="p-2">Click recorded</th>
                </tr>
              </thead>
              <tbody>
                {query.data.data.map((person) => (
                  <tr className="border-line border-t" key={person.person_id}>
                    <td className="p-2">{person.email}</td>
                    <td className="p-2">{person.sent}</td>
                    <td className="p-2">
                      {person.opens_tracked
                        ? person.opened
                        : "Not fully tracked"}
                    </td>
                    <td className="p-2">{person.clicked}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {query.data.data.length === 0 ? (
            <p className="text-fg-2 mt-3 text-sm">
              No matching sent recipients in this window.
            </p>
          ) : null}
          <div className="mt-3 flex gap-2">
            <Button disabled={!cursor} onClick={() => setCursor(null)}>
              First page
            </Button>
            <Button
              disabled={!query.data.meta.next_cursor}
              onClick={() => setCursor(query.data?.meta.next_cursor ?? null)}
            >
              Next page
            </Button>
          </div>
        </>
      ) : null}
    </section>
  );
};
