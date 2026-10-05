import {
  Alert02Icon,
  InformationCircleIcon,
  PauseIcon,
  PlayIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { IconSvgElement } from "@hugeicons/react";
import type { ConnectionObject } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import type { ReactNode } from "react";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { CopyIdItem, RowMenu } from "@/components/row-menu";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { holdUntil, useNow } from "@/features/mailboxes/parts";
import { providerInfo, providerLabel } from "@/features/mailboxes/providers";
import { connectionsKey } from "@/features/mailboxes/queries";
import { Reveal, useKept } from "@/features/mailboxes/reveal";
import { useAction } from "@/lib/actions";
import { formatDateTime } from "@/lib/format";
import { canWrite, useWorkspace } from "@/lib/workspace";
import type { Workspace } from "@/lib/workspace";

/** Whether the person may change connections: a viewer reads only, and an archived one is done. */
export const canChange = (
  workspace: Workspace,
  connection: ConnectionObject
): boolean => canWrite(workspace) && connection.status !== "archived";

/**
 * Asks for a check now. A Google or Microsoft mailbox whose grant is lost answers with its
 * consent page instead, which the browser opens (the consent comes back to this mailbox's page).
 */
const verify = async (workspace: Workspace, connection: ConnectionObject) => {
  const checked = await workspace.api.connections.verify(connection.id, {
    return_to: `/w/${workspace.slug}/mailboxes/${connection.id}`,
  });
  if (checked.authorization?.url) {
    window.location.assign(checked.authorization.url);
    return "consent";
  }
  return "checking";
};

/**
 * A mailbox's quick changes, as its page and its row offer them: check it now (a lost Google or
 * Microsoft grant opens its consent page instead), and pause or resume its sending.
 */
export const useMailboxControls = (connection: ConnectionObject) => {
  const workspace = useWorkspace();
  const action = useAction();
  const key = connectionsKey(workspace);
  return {
    handleCheck: () =>
      action(
        "Check started",
        async () => {
          await verify(workspace, connection);
        },
        key
      ),
    handleTogglePause: () =>
      action(
        connection.paused ? "Sending resumed" : "Sending paused",
        () =>
          workspace.api.connections.update(connection.id, {
            paused: !connection.paused,
          }),
        key
      ),
    pauseLabel: connection.paused ? "Resume sending" : "Pause sending",
  };
};

/**
 * What a disconnection does, from the server's rules: the account stops, its credential is
 * erased, its senders leave every campaign under each campaign's rule, its other queued mail
 * fails, and its history stays.
 */
const DisconnectText = () => (
  <div className="flex flex-col gap-2">
    <p>
      It stops sending at once: its stored password or access is erased and its
      replies are no longer read. An email already on its way finishes.
    </p>
    <p>
      Its senders leave every campaign. Each campaign moves their conversations
      to another sender or stops them, as its settings say. Direct emails and
      replies still waiting to go out from it fail.
    </p>
    <p>
      Its history stays. Connecting the same account again brings it back with
      its history and senders.
    </p>
  </div>
);

/**
 * A mailbox's actions beside its title: pause or resume its sending, and, in its menu, check it
 * now, copy its id, and disconnect it, which asks first. A viewer, or a disconnected mailbox,
 * only copies the id.
 */
export const MailboxActions = ({
  connection,
}: {
  connection: ConnectionObject;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const action = useAction();
  const controls = useMailboxControls(connection);
  const [disconnecting, setDisconnecting] = useState(false);
  const editable = canChange(workspace, connection);

  const disconnect = () =>
    action(
      "Mailbox disconnected",
      () => workspace.api.connections.delete(connection.id),
      async () => {
        await queryClient.invalidateQueries({
          queryKey: connectionsKey(workspace),
        });
        await navigate({
          params: { slug: workspace.slug },
          to: "/w/$slug/mailboxes",
        });
      }
    );

  return (
    <>
      {editable ? (
        <Button onClick={controls.handleTogglePause} variant="secondary">
          <HugeiconsIcon icon={connection.paused ? PlayIcon : PauseIcon} />
          {controls.pauseLabel}
        </Button>
      ) : null}
      <RowMenu label="More actions">
        {editable ? (
          <DropdownMenuItem onClick={controls.handleCheck}>
            Check now
          </DropdownMenuItem>
        ) : null}
        <CopyIdItem id={connection.id} noun="mailbox" />
        {editable ? (
          <DropdownMenuItem
            className="text-error-fg"
            onClick={() => setDisconnecting(true)}
          >
            Disconnect
          </DropdownMenuItem>
        ) : null}
      </RowMenu>
      <ConfirmDialog
        confirmLabel="Disconnect mailbox"
        danger
        description="What happens when it is disconnected:"
        onConfirm={disconnect}
        onOpenChange={setDisconnecting}
        open={disconnecting}
        title={`Disconnect ${connection.account.email}?`}
      >
        <div className="text-fg-2 text-sm">
          <DisconnectText />
        </div>
      </ConfirmDialog>
    </>
  );
};

/** What a notice about a mailbox's state is: its tone, its title, what it says, what to do. */
interface NoticeText {
  tone: "error" | "info" | "neutral" | "warning";
  title: string;
  text: ReactNode;
  action?: ReactNode;
  /** The icon, when the tone's own does not say it best (a pause). */
  icon?: IconSvgElement;
}

/** The states of a mailbox that need a person or that a person is waiting on, by priority. */
type StateNotice =
  | "archived"
  | "authorization_required"
  | "disabled"
  | "failed"
  | "verifying"
  | "unverified"
  | "note";

/** Which state notice a mailbox needs now, if any. */
const stateNotice = (connection: ConnectionObject): StateNotice | null => {
  switch (connection.status) {
    case "archived":
    case "authorization_required":
    case "disabled":
    case "failed":
    case "verifying":
    case "unverified": {
      return connection.status;
    }
    default: {
      return connection.status_detail ? "note" : null;
    }
  }
};

/** The API's own words about the state, before ours: what went wrong, and what to do. */
const detailed = (connection: ConnectionObject, ours: string) =>
  connection.status_detail ? (
    <>
      <span className="text-fg block">{connection.status_detail}</span>
      <span className="mt-1 block">{ours}</span>
    </>
  ) : (
    ours
  );

/** The button that opens the mailbox's settings at one panel. */
const SettingsButton = ({
  children,
  connection,
  panel,
  variant = "secondary",
}: {
  children: ReactNode;
  connection: ConnectionObject;
  panel: string;
  variant?: "primary" | "secondary";
}) => {
  const workspace = useWorkspace();
  return (
    <Button
      nativeButton={false}
      render={
        <Link
          hash={panel}
          params={{ connectionId: connection.id, slug: workspace.slug }}
          to="/w/$slug/mailboxes/$connectionId/settings"
        />
      }
      size="s"
      variant={variant}
    >
      {children}
    </Button>
  );
};

/**
 * What a state notice says. Each names what happened in plain words (with the API's own words
 * when it gave some) and offers the one action that fixes it: a new consent for Google and
 * Microsoft, a new password or credential in the settings for the others, or a new check.
 */
const useStateText = (
  connection: ConnectionObject,
  notice: StateNotice
): NoticeText => {
  const workspace = useWorkspace();
  const controls = useMailboxControls(connection);
  const editable = canChange(workspace, connection);
  const way = providerInfo(connection.provider)?.way;
  const check = (label: string, variant: "primary" | "secondary") =>
    editable ? (
      <Button onClick={controls.handleCheck} size="s" variant={variant}>
        {label}
      </Button>
    ) : null;
  switch (notice) {
    case "archived": {
      return {
        action: canWrite(workspace) ? (
          <Button
            nativeButton={false}
            render={
              <Link
                params={{ slug: workspace.slug }}
                to="/w/$slug/mailboxes/new"
              />
            }
            size="s"
            variant="secondary"
          >
            Connect it again
          </Button>
        ) : null,
        text: "It no longer sends, its stored password or access is erased, and its history stays. Connecting the same account again brings it back with its history and senders.",
        title: "Disconnected",
        tone: "neutral",
      };
    }
    case "authorization_required": {
      const login = way === "login" || way === "relay";
      let action: ReactNode = check("Reconnect", "primary");
      if (login && editable) {
        action = (
          <SettingsButton
            connection={connection}
            panel="sign-in"
            variant="primary"
          >
            {way === "login" ? "Update the password" : "Update the credential"}
          </SettingsButton>
        );
      }
      return {
        action,
        text: detailed(
          connection,
          `${providerLabel(connection.provider)} no longer accepts Norbelys's access. Until it is reconnected, it sends nothing and its conversations wait.`
        ),
        title: "Reconnect this mailbox",
        tone: "error",
      };
    }
    case "disabled": {
      return {
        action: check("Check again", "secondary"),
        text: detailed(
          connection,
          "The provider blocked the account, or too many people complained. It sends nothing until this is fixed at the provider."
        ),
        title: "Blocked",
        tone: "error",
      };
    }
    case "failed": {
      return {
        action: (
          <>
            {check("Check again", "primary")}
            {editable && (way === "login" || way === "relay") ? (
              <SettingsButton connection={connection} panel="sign-in">
                Open sign-in settings
              </SettingsButton>
            ) : null}
          </>
        ),
        text: detailed(
          connection,
          "Fix what it says, then check it again. Until then it sends nothing."
        ),
        title: "The last check failed",
        tone: "error",
      };
    }
    case "verifying": {
      return {
        text: "Norbelys is signing in to make sure it can send from this mailbox. This page updates by itself.",
        title: "Checking the connection",
        tone: "info",
      };
    }
    case "unverified": {
      return {
        action: check("Check now", "secondary"),
        text: "Its first check hasn't run yet. Check it to make sure it can send.",
        title: "Not checked yet",
        tone: "info",
      };
    }
    default: {
      return {
        text: connection.status_detail ?? "",
        title: "From its last check",
        tone: "warning",
      };
    }
  }
};

/** A notice over a mailbox: an icon, a title, what it says, then what to do. */
const Notice = ({ action, icon, text, title, tone }: NoticeText) => (
  <Alert variant={tone}>
    <HugeiconsIcon
      icon={
        icon ??
        (tone === "error" || tone === "warning"
          ? Alert02Icon
          : InformationCircleIcon)
      }
    />
    <AlertTitle>{title}</AlertTitle>
    <AlertDescription className="text-fg-2">{text}</AlertDescription>
    {/* Beside the description, not in it: its links are styled as links, and these are buttons. */}
    {action ? (
      <div className="col-start-2 mt-3 flex flex-wrap gap-2">{action}</div>
    ) : null}
  </Alert>
);

const StateNoticeView = ({
  connection,
  notice,
}: {
  connection: ConnectionObject;
  notice: StateNotice;
}) => <Notice {...useStateText(connection, notice)} />;

/** Whether a relay refuses its delivery reports: its provider signs them with a key not saved. */
const missingKey = (connection: ConnectionObject): boolean =>
  connection.status !== "archived" &&
  Boolean(providerInfo(connection.provider)?.webhookKey) &&
  connection.webhook?.key_set === false;

/**
 * The notices over a mailbox, each only while it holds: its state when it needs a person (or is
 * being checked), a hold of its provider, a person's pause, and a relay's missing webhook key.
 * Each opens and closes in place, keeping its words while it leaves, so the page below slides
 * instead of jumping when a check ends.
 */
export const MailboxNotices = ({
  connection,
}: {
  connection: ConnectionObject;
}) => {
  const workspace = useWorkspace();
  const editable = canChange(workspace, connection);
  const state = stateNotice(connection);
  const shownState = useKept(state);
  const held = holdUntil(connection.paused_until, useNow());
  const shownHeld = useKept(held);
  const paused = connection.paused && connection.status !== "archived";
  const keyLabel =
    providerInfo(connection.provider)?.webhookKey?.label ?? "webhook key";
  return (
    <div className="flex flex-col">
      <Reveal className="pb-3" open={state !== null}>
        {shownState ? (
          <StateNoticeView connection={connection} notice={shownState} />
        ) : null}
      </Reveal>
      <Reveal className="pb-3" open={held !== null}>
        <Notice
          text={`${providerLabel(connection.provider)} refused several emails in a row, so Norbelys gives it a rest. Sending resumes by itself then.`}
          title={`Waiting until ${shownHeld ? formatDateTime(shownHeld) : ""}`}
          tone="warning"
        />
      </Reveal>
      <Reveal className="pb-3" open={paused}>
        <Notice
          icon={PauseIcon}
          text="Its conversations wait, and nothing is lost. Resume sending when it should go on."
          title="Sending is paused"
          tone="neutral"
        />
      </Reveal>
      <Reveal className="pb-3" open={missingKey(connection)}>
        <Notice
          action={
            editable ? (
              <SettingsButton connection={connection} panel="delivery-reports">
                Add the key
              </SettingsButton>
            ) : null
          }
          text={`${providerLabel(connection.provider)} signs its delivery reports, and Norbelys refuses them until its ${keyLabel} is saved.`}
          title="Delivery reports are refused"
          tone="warning"
        />
      </Reveal>
    </div>
  );
};
