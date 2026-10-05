import { Badge } from "@/components/ui/badge";
import { statusLabel, statusTone } from "@/lib/status";
import type { StatusKind } from "@/lib/status";

/**
 * A state as the API reports it: a dot in its tone and its word (see `Badge`), or, with `dot`
 * false, the same word as a quiet outlined tag.
 */
export const StatusBadge = ({
  dot = true,
  kind,
  value,
}: {
  dot?: boolean;
  kind: StatusKind;
  value: string;
}) => (
  <Badge dot={dot} tone={statusTone(kind, value)}>
    {statusLabel(kind, value)}
  </Badge>
);
