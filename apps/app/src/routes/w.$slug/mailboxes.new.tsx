import { ArrowRight01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useEffect } from "react";
import { toast } from "sonner";

import { PageBody, PageHeader } from "@/components/page";
import { ConnectDialog } from "@/features/mailboxes/connect-dialog";
import { ConsentRefused, useConsentReturn } from "@/features/mailboxes/consent";
import { PROVIDER_GROUPS, PROVIDERS } from "@/features/mailboxes/providers";
import type { ProviderInfo } from "@/features/mailboxes/providers";
import { useUrlDialog } from "@/hooks/use-url-dialog";
import { useWorkspace } from "@/lib/workspace";

/** One way to connect: its mark, its name and what it does, as a row that opens its form. */
const ProviderChoice = ({
  info,
  onConnect,
}: {
  info: ProviderInfo;
  onConnect: () => void;
}) => (
  <li className="border-line border-t first:border-t-0">
    <button
      className="group hover:bg-hover focus-visible:outline-focus flex w-full cursor-pointer items-center gap-4 px-4 py-3.5 text-left transition-colors duration-(--nb-duration-micro) outline-none focus-visible:outline-1 focus-visible:-outline-offset-1"
      onClick={onConnect}
      type="button"
    >
      <span className="bg-chrome text-fg flex size-9 shrink-0 items-center justify-center rounded-sm">
        <HugeiconsIcon className="size-5" icon={info.icon} />
      </span>
      <span className="flex min-w-0 flex-1 flex-col gap-0.5">
        <span className="text-fg font-semibold">{info.name}</span>
        <span className="text-fg-3 text-xs">{info.summary}</span>
      </span>
      <HugeiconsIcon
        className="text-icon group-hover:text-fg size-4 shrink-0 transition-[color,translate] duration-(--nb-duration-micro) group-hover:translate-x-0.5 motion-reduce:transition-none"
        icon={ArrowRight01Icon}
      />
    </button>
  </li>
);

/**
 * Where a Google or Microsoft consent comes back: a connected mailbox goes on to its page; a
 * refused consent stays here with the provider's words.
 */
const useConsentLanding = () => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const consent = useConsentReturn();
  const { connectionId } = consent;
  const { slug } = workspace;
  useEffect(() => {
    if (!connectionId) {
      return;
    }
    toast.success("Mailbox connected. Its check is running.", {
      id: connectionId,
    });
    void navigate({
      params: { connectionId, slug },
      replace: true,
      to: "/w/$slug/mailboxes/$connectionId",
    });
  }, [connectionId, navigate, slug]);
  return consent;
};

/**
 * The ways to connect an account, as two short lists: the mailboxes most people connect (Google,
 * Microsoft, any other over SMTP), then the relays and the hosted mail. A choice opens its
 * provider's form in a dialog addressed by `?provider=`, so a link can open it.
 */
const ConnectPage = () => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const dialog = useUrlDialog("provider");
  const consent = useConsentLanding();
  return (
    <PageBody>
      <PageHeader
        back={{
          label: "Mailboxes",
          link: { params: { slug: workspace.slug }, to: "/w/$slug/mailboxes" },
        }}
        subtitle="Connect a personal mailbox or a sending service. Norbelys mail is available directly."
        title="Choose an email account"
      />
      <div className="flex max-w-[720px] flex-col gap-8">
        {consent.error ? (
          <ConsentRefused
            description={consent.error.description}
            onDismiss={() => consent.clear()}
            title="The mailbox was not connected"
          />
        ) : null}
        {PROVIDER_GROUPS.map((group) => (
          <section
            aria-labelledby={`${group.id}-title`}
            className="flex flex-col gap-3"
            key={group.id}
          >
            <div className="flex flex-col gap-1">
              <h2
                className="text-fg text-xl font-semibold"
                id={`${group.id}-title`}
              >
                {group.title}
              </h2>
              <p className="text-fg-2 text-sm">{group.description}</p>
            </div>
            <ul className="border-line overflow-hidden rounded-sm border">
              {group.providers.map((provider) => (
                <ProviderChoice
                  info={PROVIDERS[provider]}
                  key={provider}
                  onConnect={() => {
                    if (provider === "norbelys") {
                      void navigate({
                        params: { slug: workspace.slug },
                        to: "/w/$slug/mailboxes",
                        search: (previous) => ({
                          ...previous,
                          service: "norbelys",
                        }),
                      });
                    } else {
                      dialog.open(provider);
                    }
                  }}
                />
              ))}
            </ul>
          </section>
        ))}
      </div>
      <ConnectDialog provider={dialog.value} {...dialog.props} />
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/mailboxes/new")({
  head: () => ({ meta: [{ title: "Connect a mailbox · Norbelys" }] }),
  component: ConnectPage,
});
