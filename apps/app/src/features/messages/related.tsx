import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";

import { campaignQuery } from "@/features/campaigns/queries";
import { connectionQuery } from "@/features/mailboxes/queries";
import { personQuery } from "@/features/people/queries";
import { formatName, shortId } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/** A campaign by its name (its short id until the name loads), linking to the campaign. */
export const CampaignLink = ({ id }: { id: string }) => {
  const workspace = useWorkspace();
  const campaign = useQuery(campaignQuery(workspace, id));
  return (
    <Link
      params={{ campaignId: id, slug: workspace.slug }}
      title={id}
      to="/w/$slug/campaigns/$campaignId"
    >
      {campaign.data?.name ?? shortId(id)}
    </Link>
  );
};

/**
 * A person by name, their address under it (their short id until they load), linking to the
 * person.
 */
export const PersonLink = ({ id }: { id: string }) => {
  const workspace = useWorkspace();
  const person = useQuery(personQuery(workspace, id));
  const name = person.data ? formatName(person.data) : "";
  return (
    <span className="flex min-w-0 flex-col">
      <Link
        params={{ personId: id, slug: workspace.slug }}
        title={id}
        to="/w/$slug/people/$personId"
      >
        {name || person.data?.email || shortId(id)}
      </Link>
      {name && person.data ? (
        <span className="truncate">{person.data.email}</span>
      ) : null}
    </span>
  );
};

/**
 * A mailbox by the address it signs in as (its short id until it loads), linking to it, with the
 * identity that sends when one is named and differs from it.
 */
export const MailboxName = ({
  id,
  identityId,
}: {
  id: string;
  identityId?: string;
}) => {
  const workspace = useWorkspace();
  const mailbox = useQuery(connectionQuery(workspace, id));
  const account = mailbox.data?.account.email;
  const identity = mailbox.data?.identities.find(
    (item) => item.id === identityId
  );
  return (
    <span className="flex min-w-0 flex-col">
      <Link
        params={{ connectionId: id, slug: workspace.slug }}
        title={id}
        to="/w/$slug/mailboxes/$connectionId"
      >
        {account ?? shortId(id)}
      </Link>
      {identity && identity.email !== account ? (
        <span className="truncate">As {identity.email}</span>
      ) : null}
    </span>
  );
};
