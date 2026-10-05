import { Link } from "@tanstack/react-router";

import { CodeBlock, CodeLine } from "@/components/copy";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { Tabs, TabsList, TabsPanel, TabsTab } from "@/components/ui/tabs";
import { API_URL, DOCS, MCP_URL } from "@/lib/links";
import type { Workspace } from "@/lib/workspace";

/** One way in: its tab's label, the snippet to copy, and what to know before running it. */
const clients = (workspace: Workspace) =>
  [
    {
      code: `curl ${API_URL}/v1/campaigns \\
  -H "Authorization: Bearer $NORBELYS_API_KEY"`,
      hint: "Create an API key for this workspace and keep it in your environment, never in code.",
      id: "curl",
      label: "cURL",
    },
    {
      code: `import { Norbelys } from "@norbelys/sdk";

// Reads NORBELYS_API_KEY; keys belong to one workspace (${workspace.slug}).
const norbelys = new Norbelys();

for await (const campaign of norbelys.campaigns.list({ limit: 20 })) {
  console.log(campaign.name, campaign.status);
}`,
      hint: "Create an API key for this workspace and keep it in your environment, never in code.",
      id: "typescript",
      label: "TypeScript",
    },
    {
      code: `norbelys login
norbelys campaigns list --limit 20`,
      hint: "`norbelys login` approves this device in the browser; no key is pasted anywhere.",
      id: "cli",
      label: "CLI",
    },
    {
      code: `claude mcp add --transport http norbelys ${MCP_URL}`,
      hint: "Give Claude, Cursor or any MCP client access: the server signs your agent in with OAuth and asks which workspace it may act in.",
      id: "mcp",
      label: "AI agents (MCP)",
    },
  ] as const;

/**
 * Everything a program or an AI agent needs to reach this workspace: the API's address, the
 * workspace's id, and a snippet for each way in (HTTP, the SDK, the CLI, MCP). The dashboard
 * itself is one more client of the same public API.
 */
export const ConnectPanel = ({
  createKey = true,
  workspace,
}: {
  /** Whether the panel offers to create a key (not on the API keys page, which has its own). */
  createKey?: boolean;
  workspace: Workspace;
}) => {
  const ways = clients(workspace);
  return (
    <Card>
      <CardHeader className="flex-row items-start justify-between gap-4">
        <div className="flex flex-col gap-0.5">
          <CardTitle>Connect your code</CardTitle>
          <CardDescription>
            The same public API the dashboard uses, from your code, the terminal
            or an AI agent.
          </CardDescription>
        </div>
        {createKey ? (
          <Button
            render={
              <Link
                params={{ slug: workspace.slug }}
                search={{ new: true }}
                to="/w/$slug/api-keys"
              />
            }
            size="s"
            variant="primary"
          >
            Create API key
          </Button>
        ) : null}
      </CardHeader>
      <CardContent className="flex flex-col gap-4">
        <div className="grid gap-4 sm:grid-cols-2">
          <div className="flex min-w-0 flex-col gap-1.5">
            <Label>API base URL</Label>
            <CodeLine prefix={null} value={API_URL} />
          </div>
          <div className="flex min-w-0 flex-col gap-1.5">
            <Label>Workspace ID</Label>
            <CodeLine prefix={null} value={workspace.id} />
          </div>
        </div>
        <Tabs defaultValue="curl">
          <TabsList>
            {ways.map((way) => (
              <TabsTab key={way.id} value={way.id}>
                {way.label}
              </TabsTab>
            ))}
          </TabsList>
          {ways.map((way) => (
            <TabsPanel
              className="flex flex-col gap-2 pt-3"
              key={way.id}
              value={way.id}
            >
              <CodeBlock value={way.code} />
              <p className="text-fg-3 text-xs">
                {way.hint}{" "}
                <a
                  className="text-link hover:text-link-hover font-semibold"
                  href={DOCS}
                  rel="noreferrer"
                  target="_blank"
                >
                  Docs
                </a>
              </p>
            </TabsPanel>
          ))}
        </Tabs>
      </CardContent>
    </Card>
  );
};
