import type { MessageObject } from "@norbelys/sdk";

import { StatusBadge } from "@/components/status-badge";
import { Badge } from "@/components/ui/badge";

/** Recipient delivery evidence overrides the submission label without changing retry state. */
export const MessageStatus = ({ message }: { message: MessageObject }) => {
  switch (message.delivery?.status) {
    case "blocked": {
      return (
        <Badge dot tone="warning">
          Blocked by provider
        </Badge>
      );
    }
    case "delivered": {
      return (
        <Badge dot tone="success">
          Delivered
        </Badge>
      );
    }
    case "failed": {
      return (
        <Badge dot tone="error">
          Failed
        </Badge>
      );
    }
    case "partial": {
      return (
        <Badge dot tone="warning">
          Partial delivery
        </Badge>
      );
    }
    default: {
      return message.state === "sent" ? (
        <Badge dot tone="neutral">
          Awaiting delivery confirmation
        </Badge>
      ) : (
        <StatusBadge kind="message" value={message.state} />
      );
    }
  }
};
