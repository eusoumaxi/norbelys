import { Link04Icon } from "@hugeicons/core-free-icons";
import { useState } from "react";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { CodeBlock } from "@/components/copy";
import { Dash, DataTable } from "@/components/data-table";
import { RowMenu } from "@/components/row-menu";
import { SettingsPanel } from "@/components/settings-layout";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { useAction } from "@/lib/actions";
import { useRefreshMe, useSession } from "@/lib/auth";
import { formatRelative } from "@/lib/format";
import type { Me } from "@/lib/session";

type LinkedIdentity = Me["identities"][number];

/**
 * Owner recovery codes: registered before a workspace enforces single sign-on, one of them lets an
 * operator open an audited repair session if the identity provider locks every owner out. A new
 * set replaces the old one and is shown once.
 */
export const RecoveryCodes = () => {
  const session = useSession();
  const act = useAction();
  const [codes, setCodes] = useState<string[] | null>(null);
  const [confirming, setConfirming] = useState(false);
  const owner = session.memberships.some((m) => m.role === "owner");
  if (!owner) {
    return null;
  }
  return (
    <SettingsPanel
      description="If a workspace you own enforces single sign-on and the provider ever locks every owner out, one of these codes lets the Norbelys operators open a short, audited session to repair it. Keep them offline."
      title="Recovery codes"
    >
      {codes ? (
        <Alert variant="success">
          <AlertTitle>Your recovery codes</AlertTitle>
          <AlertDescription className="flex flex-col gap-2">
            Save them now: they are shown once, and the previous ones no longer
            work.
            <CodeBlock value={codes.join("\n")} />
          </AlertDescription>
        </Alert>
      ) : (
        <div>
          <Button onClick={() => setConfirming(true)} variant="secondary">
            Create recovery codes
          </Button>
        </div>
      )}
      <ConfirmDialog
        confirmLabel="Create codes"
        description="A new set replaces any codes you made before."
        onConfirm={() => {
          act("Recovery codes created", async () => {
            const created = await session.createRecoveryCodes();
            setCodes(created.codes);
          });
        }}
        onOpenChange={setConfirming}
        open={confirming}
        title="Create recovery codes?"
      />
    </SettingsPanel>
  );
};

/** Sign-in accounts at other providers (Google, a workspace's SSO) linked to this person. */
export const LinkedIdentities = () => {
  const session = useSession();
  const refresh = useRefreshMe();
  const act = useAction();
  const { identities } = session.me;
  if (identities.length === 0) {
    return null;
  }
  return (
    <SettingsPanel
      description="Accounts at identity providers that sign you in. Unlinking one stops it from signing you in."
      title="Linked accounts"
    >
      <DataTable<LinkedIdentity>
        columns={[
          {
            header: "Provider",
            id: "issuer",
            render: (identity) => (
              <span className="text-fg font-mono text-xs">
                {identity.issuer}
              </span>
            ),
          },
          {
            header: "Email",
            id: "email",
            render: (identity) => identity.email ?? <Dash />,
          },
          {
            header: "Linked",
            id: "linked",
            render: (identity) => formatRelative(identity.created_at),
          },
          {
            className: "w-[62px]",
            header: "",
            id: "menu",
            render: (identity) => (
              <RowMenu>
                <DropdownMenuItem
                  className="text-error-fg"
                  onClick={() =>
                    act(
                      "Account unlinked",
                      () => session.unlinkIdentity(identity.id),
                      refresh
                    )
                  }
                >
                  Unlink
                </DropdownMenuItem>
              </RowMenu>
            ),
          },
        ]}
        empty={{ description: "", icon: Link04Icon, title: "" }}
        rowKey={(identity) => identity.id}
        rows={identities}
      />
    </SettingsPanel>
  );
};
