import type { DnsRecord } from "@norbelys/sdk";

import { CopyButton } from "@/components/copy";
import { StatusBadge } from "@/components/status-badge";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { humanize } from "@/lib/format";

/** What each record proves or enables. */
const PURPOSES: Record<string, string> = {
  dkim: "DKIM key of the hosted mail",
  dmarc: "Policy for mail that fails authentication",
  ownership: "Proves you own the domain",
  mx: "Routes incoming email to the hosted mailbox",
  spf: "Authorizes the hosted mail server",
  tracking: "Serves tracking links",
};

/** A record's value in monospace, cut to its cell with the whole of it in the title, and copied. */
const Value = ({ label, value }: { label: string; value: string }) => (
  <span className="flex min-w-0 items-center gap-1.5">
    <code className="text-fg min-w-0 truncate font-mono text-xs" title={value}>
      {value}
    </code>
    <CopyButton label={`Copy ${label}`} value={value} />
  </span>
);

/**
 * The DNS records a sending domain publishes (ownership, its tracking CNAME when it serves
 * tracking links, and the hosted mail's SPF, DMARC and DKIM), each with copy buttons, publication
 * advice and what the last check found. SPF and DMARC instructions remain unchecked.
 */
export const DomainRecordsTable = ({
  records,
}: {
  records: readonly DnsRecord[];
}) => (
  <Table shell>
    <TableHeader>
      <TableRow>
        <TableHead className="w-[80px]">Type</TableHead>
        <TableHead>Name</TableHead>
        <TableHead>Value</TableHead>
        <TableHead>Priority</TableHead>
        <TableHead>Purpose</TableHead>
        <TableHead>Last check</TableHead>
      </TableRow>
    </TableHeader>
    <TableBody>
      {records.map((record) => (
        <TableRow key={`${record.type}:${record.name}:${record.purpose}`}>
          <TableCell className="font-mono text-xs">{record.type}</TableCell>
          <TableCell className="max-w-[240px]">
            <Value label="name" value={record.name} />
          </TableCell>
          <TableCell className="max-w-[320px]">
            <Value label="value" value={record.value} />
          </TableCell>
          <TableCell className="font-mono text-xs">
            {record.priority ?? "—"}
          </TableCell>
          <TableCell className="text-fg-2">
            {PURPOSES[record.purpose] ?? humanize(record.purpose)}
            {record.note ? (
              <p className="text-fg-3 mt-1 text-xs">{record.note}</p>
            ) : null}
            {record.purpose === "spf" &&
            records.some(
              (other) => other.type === "CNAME" && other.name === record.name
            ) ? (
              <p className="text-fg-3 mt-1 text-xs">
                This hostname serves tracking through a CNAME, which cannot
                coexist with an SPF TXT record. Register a separate sending
                domain with tracking disabled for managed mail.
              </p>
            ) : null}
          </TableCell>
          <TableCell>
            <StatusBadge kind="record" value={record.status} />
          </TableCell>
        </TableRow>
      ))}
    </TableBody>
  </Table>
);
