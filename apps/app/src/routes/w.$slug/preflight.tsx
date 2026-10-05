import { Copy01Icon, Mail01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { Finding } from "@norbelys/sdk";
import { useMutation } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";

import { DataTable, Dash } from "@/components/data-table";
import { MetricGroup } from "@/components/details";
import { PageBody, PageHeader, Section } from "@/components/page";
import { ProblemPanel } from "@/components/problem";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardFooter,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Spinner } from "@/components/ui/spinner";
import { Textarea } from "@/components/ui/textarea";
import { copyText } from "@/lib/actions";
import { formatCount, formatDate, humanize } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/** The most addresses one check takes, as the API bounds it. */
const LIMIT = 100;

/** The addresses typed or pasted: one per line (commas and semicolons separate too), once each. */
const parseAddresses = (text: string): string[] => [
  ...new Set(
    text
      .split(/[\n,;]+/u)
      .map((line) => line.trim())
      .filter(Boolean)
  ),
];

/** Why a verdict was given, in words; a reason newer than this page is shown as it comes. */
const REASONS: Record<string, string> = {
  dns_unavailable: "DNS did not answer: check again later",
  implicit_mx: "No MX record; the domain receives its own mail",
  mx: "The domain publishes MX records",
  no_domain: "The domain does not exist",
  no_route: "The domain has neither MX nor address records",
  null_mx: "The domain publishes a null MX: it accepts no mail",
  syntax: "Not a valid address",
};

/** The verdict and its reason; a syntax problem adds what is wrong. */
const Reason = ({
  finding,
  testMode,
}: {
  finding: Finding;
  testMode: boolean;
}) => (
  <span className="flex min-w-0 flex-col">
    <span className="text-fg-2">
      {/* A test-mode workspace checks the syntax only, though the API still answers `mx`. */}
      {testMode && finding.reason === "mx"
        ? "Valid syntax (test mode: DNS not checked)"
        : (REASONS[finding.reason] ?? humanize(finding.reason))}
    </span>
    {finding.detail ? (
      <span className="text-fg-3 text-xs">{finding.detail}</span>
    ) : null}
  </span>
);

/** What the workspace itself says about the address: suppressed, or held until a review. */
const WorkspaceFlags = ({ finding }: { finding: Finding }) => {
  if (!finding.suppression && !finding.hold) {
    return <Dash />;
  }
  return (
    <span className="flex flex-wrap items-center gap-1.5">
      {finding.suppression ? (
        <Badge dot tone="warning" title={finding.suppression.id}>
          Suppressed · {humanize(finding.suppression.reason)}
        </Badge>
      ) : null}
      {finding.hold ? (
        <Badge
          dot
          tone="warning"
          title={`Held until a review after ${formatDate(finding.hold.review_after)}`}
        >
          Held · {humanize(finding.hold.reason)}
        </Badge>
      ) : null}
    </span>
  );
};

/** Whether the workspace would mail the address: routable, and neither suppressed nor held. */
const mailable = (finding: Finding): boolean =>
  finding.status === "routable" && !finding.suppression && !finding.hold;

/** The counts of a check's verdicts, and the findings as a table. */
const Results = ({ findings }: { findings: Finding[] }) => {
  const testMode = useWorkspace().mode === "test";
  const count = (status: string) =>
    formatCount(findings.filter((f) => f.status === status).length);
  const ready = findings.filter(mailable).map((f) => f.email);
  return (
    <Section
      actions={
        <Button
          disabled={ready.length === 0}
          onClick={() =>
            copyText(
              ready.join("\n"),
              `${formatCount(ready.length)} addresses copied`
            )
          }
          size="s"
          variant="secondary"
        >
          <HugeiconsIcon icon={Copy01Icon} />
          Copy mailable addresses
        </Button>
      }
      title="Results"
    >
      <MetricGroup
        metrics={[
          { label: "Routable", value: count("routable") },
          { label: "Invalid", value: count("invalid") },
          { label: "Unknown", value: count("unknown") },
          {
            label: "Suppressed or held",
            value: formatCount(
              findings.filter((f) => f.suppression || f.hold).length
            ),
          },
        ]}
      />
      <DataTable<Finding>
        columns={[
          {
            render: (f) => (
              <span className="text-fg block max-w-[320px] truncate font-mono text-xs font-medium">
                {f.email}
              </span>
            ),
            header: "Address",
            id: "email",
          },
          {
            render: (f) => <StatusBadge kind="verdict" value={f.status} />,
            header: "Verdict",
            id: "status",
          },
          {
            render: (f) => <Reason finding={f} testMode={testMode} />,
            header: "Reason",
            id: "reason",
          },
          {
            render: (f) => <WorkspaceFlags finding={f} />,
            header: "Workspace",
            id: "workspace",
          },
        ]}
        rowKey={(f) => f.email}
        rows={findings}
      />
    </Section>
  );
};

/** What the count line under the addresses says: how many, or how many too many. */
const CountLine = ({ count }: { count: number }) => {
  if (count > LIMIT) {
    return (
      <span className="text-error-fg text-xs">
        {formatCount(count)} addresses: check at most {LIMIT} at a time.
      </span>
    );
  }
  return (
    <span className="text-fg-3 text-xs tabular-nums">
      {formatCount(count)} of {LIMIT} addresses
    </span>
  );
};

/**
 * Checks addresses before they are mailed: syntax, the domain's mail routing in DNS (MX, an
 * implicit MX, or a null MX that refuses all mail), and the workspace's suppressions and holds.
 * It never probes a mailbox: nothing is sent and nothing is stored.
 */
const PreflightPage = () => {
  const workspace = useWorkspace();
  const [text, setText] = useState("");
  const addresses = parseAddresses(text);
  const check = useMutation({
    mutationFn: (emails: string[]) =>
      workspace.api.preflight.create({ emails }),
  });
  return (
    <PageBody>
      <PageHeader title="Address check" />
      <div className="flex max-w-[1000px] flex-col gap-6">
        <p className="text-fg-2 text-sm">
          Checks each address&apos;s syntax and its domain&apos;s mail routing
          in DNS (MX records, an implicit MX, or a null MX that refuses all
          mail), and whether this workspace suppresses or holds it. It never
          probes a mailbox: nothing is sent to the addresses and nothing is
          stored. A domain this workspace mailed in the last day is answered as
          its sending saw it.
        </p>
        {workspace.mode === "test" ? (
          <Alert variant="info">
            <AlertTitle>Test mode</AlertTitle>
            <AlertDescription>
              Only the syntax is checked, as this workspace&apos;s sending does:
              its mail never leaves the test transport.
            </AlertDescription>
          </Alert>
        ) : null}
        <Card>
          <CardHeader className="flex-col items-start gap-0.5">
            <CardTitle>Addresses</CardTitle>
            <CardDescription>
              One per line; commas and semicolons separate them too.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <Textarea
              aria-label="Addresses to check"
              className="min-h-40 font-mono"
              onChange={(event) => setText(event.target.value)}
              placeholder={"ada@example.com\ngrace@example.org"}
              rows={8}
              spellCheck={false}
              value={text}
            />
          </CardContent>
          <CardFooter className="justify-between gap-3">
            <CountLine count={addresses.length} />
            <div className="flex items-center gap-2">
              {text ? (
                <Button
                  onClick={() => {
                    setText("");
                    check.reset();
                  }}
                  variant="tertiary"
                >
                  Clear
                </Button>
              ) : null}
              <Button
                disabled={
                  check.isPending ||
                  addresses.length === 0 ||
                  addresses.length > LIMIT
                }
                onClick={() => check.mutate(addresses)}
                variant="primary"
              >
                {check.isPending ? (
                  <Spinner />
                ) : (
                  <HugeiconsIcon icon={Mail01Icon} />
                )}
                Check addresses
              </Button>
            </div>
          </CardFooter>
        </Card>
        {check.isError ? (
          <ProblemPanel
            error={check.error}
            onRetry={() => check.mutate(addresses)}
          />
        ) : null}
        {check.data ? <Results findings={check.data.data} /> : null}
      </div>
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/preflight")({
  head: () => ({ meta: [{ title: "Address check · Norbelys" }] }),
  component: PreflightPage,
});
