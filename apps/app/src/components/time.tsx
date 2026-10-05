import { formatRelative, formatTimestamp } from "@/lib/format";

/** How long ago (or how soon) a moment is, the moment itself on hover: `2 hours ago`. */
export const RelativeTime = ({
  className,
  value,
}: {
  className?: string;
  value: string;
}) => (
  <time className={className} dateTime={value} title={formatTimestamp(value)}>
    {formatRelative(value)}
  </time>
);

/** A moment and how long ago (or how soon) it is: `2026-10-02 16:37:10 · 2 hours ago`. */
export const When = ({ value }: { value: string }) => (
  <time dateTime={value}>
    {formatTimestamp(value)}{" "}
    <span className="text-fg-3">· {formatRelative(value)}</span>
  </time>
);
