import { createFileRoute } from "@tanstack/react-router";

import { PageBody, PageHeader } from "@/components/page";
import { ComposeForm } from "@/features/messages/compose-form";
import { useWorkspace } from "@/lib/workspace";

/**
 * A direct message written by hand: queued at once, sent when due from one of the workspace's
 * sender identities, outside any campaign's cadence.
 */
const NewMessagePage = () => {
  const workspace = useWorkspace();
  return (
    <PageBody>
      <PageHeader
        compact
        back={{
          label: "Messages",
          link: {
            params: { slug: workspace.slug },
            to: "/w/$slug/messages",
          },
        }}
        title="Send message"
      />
      <ComposeForm />
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/messages/new")({
  head: () => ({ meta: [{ title: "Send message · Norbelys" }] }),
  component: NewMessagePage,
});
