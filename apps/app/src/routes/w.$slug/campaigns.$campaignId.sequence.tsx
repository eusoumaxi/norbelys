import { createFileRoute } from "@tanstack/react-router";

import { campaignSectionHead } from "@/features/campaigns/format";
import { SequenceEditor } from "@/features/campaigns/sequence/sequence-editor";

export const Route = createFileRoute("/w/$slug/campaigns/$campaignId/sequence")(
  {
    head: ({ matches }) => campaignSectionHead(matches, "Sequence"),
    component: SequenceEditor,
  }
);
