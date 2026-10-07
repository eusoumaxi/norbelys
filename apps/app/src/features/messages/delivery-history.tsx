import { useInfiniteQuery } from "@tanstack/react-query";
import type { ReactNode } from "react";

import { Problem } from "@/components/problem";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { messageEventsQuery } from "@/features/messages/queries";
import { formatTimestamp, humanize } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/**
 * The delivery events of a message, oldest first: what each provider reported, and when. A
 * temporary deferral is said as such, never as a bounce; provider policy blocks are flagged.
 */
export const DeliveryHistory = ({ messageId }: { messageId: string }) => {
  const workspace = useWorkspace();
  const events = useInfiniteQuery(messageEventsQuery(workspace, messageId));
  const history = events.data?.pages.flatMap((page) => page.data) ?? [];
  // What stands in for the list while it loads, fails or is empty.
  let body: ReactNode = null;
  if (events.isError) {
    body = (
      <Problem
        error={events.error}
        onRetry={() => {
          void events.refetch();
        }}
      />
    );
  } else if (events.isPending) {
    body = (
      <div className="flex flex-col gap-3 px-4 pb-4">
        <Skeleton className="h-4 w-1/2" />
        <Skeleton className="h-4 w-2/3" />
      </div>
    );
  } else if (history.length === 0) {
    body = (
      <p className="text-fg-3 border-line border-t px-4 py-6 text-sm">
        No delivery events have been reported yet.
      </p>
    );
  }
  return (
    <Card>
      <CardHeader className="flex-col items-start gap-0.5">
        <CardTitle>Delivery history</CardTitle>
        <CardDescription className="text-xs">
          A temporary deferral is not a final bounce. Provider observations may
          arrive later.
        </CardDescription>
      </CardHeader>
      <CardContent className="px-0 pb-0">
        {body ?? (
          <ol className="border-line border-t">
            {history.map((event) => (
              <li
                className="border-line border-b px-4 py-3 last:border-b-0"
                key={event.id}
              >
                <div className="flex flex-wrap items-center justify-between gap-2">
                  <span className="flex items-center gap-2 text-sm font-semibold">
                    {humanize(event.kind)}
                    {event.category === "policy" ? (
                      <Badge tone="warning">
                        {event.provider_code === "JFE050005"
                          ? "Account restriction"
                          : "Provider policy"}
                      </Badge>
                    ) : null}
                  </span>
                  <time
                    className="text-fg-3 text-xs"
                    dateTime={event.observed_at}
                  >
                    {formatTimestamp(event.observed_at)}
                  </time>
                </div>
                <p className="text-fg-3 mt-1 text-xs">
                  {[event.recipient, event.enhanced_status, event.source]
                    .filter(Boolean)
                    .join(" · ")}
                </p>
                {event.provider_code ? (
                  <p className="text-fg mt-2 font-mono text-xs">
                    Provider code: {event.provider_code}
                  </p>
                ) : null}
                {event.diagnostic ? (
                  <p className="text-fg-2 mt-1 text-sm break-words">
                    {event.diagnostic}
                  </p>
                ) : null}
                {event.provider_code === "JFE050005" ? (
                  <p className="text-warning mt-1 text-xs">
                    This is an account restriction, not proof that this variant
                    caused the refusal. Sending-provider acceptance does not
                    confirm delivery to the recipient.
                  </p>
                ) : null}
              </li>
            ))}
          </ol>
        )}
        {events.hasNextPage ? (
          <div className="border-line flex justify-center border-t p-3">
            <Button
              disabled={events.isFetchingNextPage}
              onClick={() => {
                void events.fetchNextPage();
              }}
              size="s"
              variant="secondary"
            >
              {events.isFetchingNextPage ? <Spinner /> : null}
              Load more events
            </Button>
          </div>
        ) : null}
      </CardContent>
    </Card>
  );
};
