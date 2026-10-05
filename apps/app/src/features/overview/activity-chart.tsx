import type { AnalyticsGroup, AnalyticsObject, Counters } from "@norbelys/sdk";
import type { UseQueryResult } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";

import { Problem } from "@/components/problem";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import {
  DAY_MS,
  formatCompact,
  formatCount,
  formatTimestamp,
  utcDay,
} from "@/lib/format";

/** What a report not counted yet says: a missing report is not zero. */
export const WAITING_FOR_REPORT =
  "Waiting for the first report. Counters refresh every few minutes.";

/** Each counter of a report in words: opens, clicks and replies count people, not robots. */
export const COUNTER_LABELS: Record<keyof Counters, string> = {
  bounced: "Bounced",
  clicked: "Human clicks",
  complained: "Complaints",
  delivered: "Delivered",
  opened: "Human opens",
  replied: "Human replies",
  sent: "Sent",
  unsubscribed: "Unsubscribes",
};

const HEIGHT = 240;
const PAD = { bottom: 24, left: 40, right: 8, top: 8 };
const BAR_MAX = 24;

/** Clean ticks for the y-axis: 0 and up to four round steps covering `max`. */
const ticks = (max: number): number[] => {
  if (max <= 0) {
    return [0, 1];
  }
  const rough = max / 4;
  const power = 10 ** Math.floor(Math.log10(rough));
  const step =
    [1, 2, 2.5, 5, 10].map((m) => m * power).find((s) => s >= rough) ??
    power * 10;
  const top = Math.ceil(max / step) * step;
  return Array.from({ length: Math.round(top / step) + 1 }, (_, i) => i * step);
};

const dayLabel = (day: string) =>
  new Date(`${day}T00:00:00Z`).toLocaleDateString("en", {
    day: "numeric",
    month: "short",
    timeZone: "UTC",
  });

/** Every day of the range, the missing ones as zero, so gaps read as quiet days. */
const fillDays = (from: string, to: string, data: AnalyticsGroup[]) => {
  const byDay = new Map(
    data.filter((g) => g.day).map((g) => [g.day as string, g.counters])
  );
  const days: { day: string; counters: Counters | undefined }[] = [];
  for (
    let t = Date.parse(`${from}T00:00:00Z`);
    t <= Date.parse(`${to}T00:00:00Z`);
    t += DAY_MS
  ) {
    const day = utcDay(t);
    days.push({ counters: byDay.get(day), day });
  }
  return days;
};

/** The counters the tooltip and the hidden table list, in the order a message lives them. */
const ROWS: (keyof Counters)[] = [
  "sent",
  "delivered",
  "bounced",
  "replied",
  "opened",
  "clicked",
];

/**
 * One counter per day (UTC; messages sent unless `metric` says otherwise), one column per day with a 4px rounded top, and a tooltip with the
 * day's counters. One series, so the card's title names it and no legend is drawn; a hidden table
 * carries the same figures for screen readers.
 */
export const ActivityChart = ({
  data,
  from,
  height = HEIGHT,
  metric = "sent",
  to,
}: {
  data: AnalyticsGroup[];
  from: string;
  /** The chart's height in pixels, axes included. */
  height?: number;
  /** The counter the bars draw; the tooltip lists them all. */
  metric?: keyof Counters;
  to: string;
}) => {
  const box = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(800);
  const [hover, setHover] = useState<number | null>(null);
  useEffect(() => {
    const element = box.current;
    if (!element) {
      return;
    }
    const observer = new ResizeObserver(([entry]) => {
      if (entry) {
        setWidth(entry.contentRect.width);
      }
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const days = fillDays(from, to, data);
  const max = Math.max(0, ...days.map((d) => d.counters?.[metric] ?? 0));
  const scale = ticks(max);
  const top = scale.at(-1) ?? 1;
  const plotW = Math.max(width - PAD.left - PAD.right, 10);
  const plotH = height - PAD.top - PAD.bottom;
  const band = plotW / Math.max(days.length, 1);
  const barW = Math.min(BAR_MAX, Math.max(band - 2, 2));
  const y = (value: number) => PAD.top + plotH - (value / top) * plotH;
  const labelEvery = Math.max(
    1,
    Math.ceil(days.length / Math.max(plotW / 64, 1))
  );
  const active = hover === null ? null : days[hover];

  return (
    <div className="relative min-w-0" ref={box}>
      <svg
        aria-hidden
        className="block w-full max-w-full overflow-visible"
        height={height}
        onMouseLeave={() => setHover(null)}
        width={width}
      >
        {scale.map((value) => (
          <g key={value}>
            <line
              className="stroke-line"
              strokeWidth={1}
              x1={PAD.left}
              x2={width - PAD.right}
              y1={y(value) + 0.5}
              y2={y(value) + 0.5}
            />
            <text
              className="fill-fg-3 text-axis"
              dominantBaseline="middle"
              textAnchor="end"
              x={PAD.left - 8}
              y={y(value)}
            >
              {formatCompact(value)}
            </text>
          </g>
        ))}
        {days.map((d, index) => {
          const sent = d.counters?.[metric] ?? 0;
          const x = PAD.left + index * band + (band - barW) / 2;
          const h = Math.max(PAD.top + plotH - y(sent), sent > 0 ? 2 : 0);
          const r = Math.min(4, barW / 2, h);
          const base = PAD.top + plotH;
          return (
            <g key={d.day}>
              {h > 0 ? (
                <path
                  className="fill-chart"
                  d={`M${x},${base} V${base - h + r} Q${x},${base - h} ${x + r},${base - h} H${x + barW - r} Q${x + barW},${base - h} ${x + barW},${base - h + r} V${base} Z`}
                  opacity={hover === null || hover === index ? 1 : 0.45}
                />
              ) : null}
              {index % labelEvery === 0 ? (
                <text
                  className="fill-fg-3 text-axis"
                  textAnchor="middle"
                  x={PAD.left + index * band + band / 2}
                  y={height - 6}
                >
                  {dayLabel(d.day)}
                </text>
              ) : null}
              <rect
                fill="transparent"
                height={plotH}
                onMouseEnter={() => setHover(index)}
                width={band}
                x={PAD.left + index * band}
                y={PAD.top}
              />
            </g>
          );
        })}
      </svg>
      {active && hover !== null ? (
        <div
          className="border-line bg-surface shadow-menu pointer-events-none absolute top-2 z-10 w-48 rounded-sm border px-3 py-2"
          style={{
            left: Math.min(
              Math.max(PAD.left + hover * band + band / 2 - 96, 0),
              width - 192
            ),
          }}
        >
          <p className="text-fg mb-1 text-xs font-semibold">
            {dayLabel(active.day)}
          </p>
          <dl className="flex flex-col gap-0.5">
            {ROWS.map((key) => (
              <div
                className="flex items-center justify-between gap-3 text-xs"
                key={key}
              >
                <dt className="text-fg-2 flex items-center gap-1.5">
                  {key === metric ? (
                    <span aria-hidden className="bg-chart size-2 rounded-xs" />
                  ) : null}
                  {COUNTER_LABELS[key]}
                </dt>
                <dd className="text-fg tabular-nums">
                  {formatCount(active.counters?.[key] ?? 0)}
                </dd>
              </div>
            ))}
          </dl>
        </div>
      ) : null}
      <div className="sr-only">
        <table>
          <caption>Messages per day</caption>
          <thead>
            <tr>
              <th>Day</th>
              {ROWS.map((key) => (
                <th key={key}>{COUNTER_LABELS[key]}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {days.map((d) => (
              <tr key={d.day}>
                <td>{d.day}</td>
                {ROWS.map((key) => (
                  <td key={key}>{d.counters?.[key] ?? 0}</td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
};

/** The chart of a report, or why there is none: loading, a failure, or no report yet. */
const ActivityBody = ({
  analytics,
}: {
  analytics: UseQueryResult<AnalyticsObject>;
}) => {
  if (analytics.isError) {
    return (
      <Problem
        error={analytics.error}
        onRetry={() => {
          void analytics.refetch();
        }}
      />
    );
  }
  if (!analytics.data) {
    return <Skeleton className="h-60 w-full" />;
  }
  if (!analytics.data.computed_at) {
    return (
      <p className="text-fg-3 grid h-60 place-items-center text-sm">
        {WAITING_FOR_REPORT}
      </p>
    );
  }
  return (
    <ActivityChart
      data={analytics.data.data}
      from={analytics.data.from}
      to={analytics.data.to}
    />
  );
};

/**
 * The messages sent per UTC day over the last 30 days, in a card: a workspace's or a campaign's,
 * as `analytics` reads them from the rollup, with how far the report counted.
 */
export const ActivityCard = ({
  analytics,
}: {
  analytics: UseQueryResult<AnalyticsObject>;
}) => {
  const counted = analytics.data?.computed_at;
  return (
    <Card>
      <CardHeader className="flex-col items-start gap-0.5">
        <CardTitle>Messages sent</CardTitle>
        <CardDescription className="text-xs">
          {counted
            ? `Per day (UTC), the last 30 days, counted through ${formatTimestamp(counted)}`
            : "Per day (UTC), the last 30 days"}
        </CardDescription>
      </CardHeader>
      <CardContent>
        <ActivityBody analytics={analytics} />
      </CardContent>
    </Card>
  );
};
