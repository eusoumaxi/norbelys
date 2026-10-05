import { PageHeader } from "@/components/page";
import { TabLinks } from "@/components/ui/tabs";
import { useWorkspace } from "@/lib/workspace";

/**
 * The inbox's title and its two views as route tabs: the conversations (threads, one per person
 * and sender) and every message the inbox read, with its classification and review.
 */
export const InboxHeader = () => {
  const workspace = useWorkspace();
  const params = { slug: workspace.slug };
  return (
    <>
      <PageHeader compact title="Inbox" />
      <TabLinks
        className="mb-5"
        tabs={[
          {
            exact: true,
            label: "Conversations",
            link: { params, to: "/w/$slug/inbox" },
          },
          {
            label: "Received mail",
            link: { params, to: "/w/$slug/inbox/received" },
          },
        ]}
      />
    </>
  );
};
