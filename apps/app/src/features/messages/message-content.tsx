import type { MessageContent, MessageObject } from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";
import type { ReactNode } from "react";

import { Problem } from "@/components/problem";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsList, TabsPanel, TabsTab } from "@/components/ui/tabs";
import { MailFrame } from "@/features/messages/body-editor";
import { messageContentQuery } from "@/features/messages/queries";
import { formatTimestamp } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/** Download the retained body unchanged; the visual view alone disables active content. */
const downloadBody = (id: string, body: string, html: boolean) => {
  const url = URL.createObjectURL(
    new Blob([body], {
      type: html ? "text/html;charset=utf-8" : "text/plain;charset=utf-8",
    })
  );
  const link = document.createElement("a");
  link.href = url;
  link.download = `${id}.${html ? "html" : "txt"}`;
  link.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
};

const Source = ({ text }: { text: string }) => (
  <pre className="bg-chrome max-h-[36rem] overflow-auto rounded-sm p-4 font-mono text-xs break-words whitespace-pre-wrap">
    {text}
  </pre>
);

/** Never re-evaluate templates here: these are the MIME parts retained for this message. */
const StoredBody = ({ content }: { content: MessageContent }) => {
  const { html = null, text = null } = content;
  const hasBody = html !== null || text !== null;
  if (!hasBody) {
    return (
      <p className="text-fg-3 py-4 text-sm">
        No message body has been retained yet. Campaign content appears after
        the sender prepares it for sending.
      </p>
    );
  }
  return (
    <Tabs
      defaultValue={html === null ? "text" : "email"}
      key={html === null ? "text" : "html"}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <TabsList aria-label="Message content">
          {html === null ? null : <TabsTab value="email">Email</TabsTab>}
          {html === null ? null : <TabsTab value="html">HTML source</TabsTab>}
          {text === null ? null : <TabsTab value="text">Plain text</TabsTab>}
          {content.headers.length > 0 ? (
            <TabsTab value="headers">Headers</TabsTab>
          ) : null}
        </TabsList>
        <Button
          onClick={() =>
            downloadBody(content.id, html ?? text ?? "", html !== null)
          }
          size="s"
          variant="secondary"
        >
          Download {html === null ? "text" : "HTML"}
        </Button>
      </div>
      {html === null ? null : (
        <>
          <TabsPanel value="email">
            <p className="text-fg-3 mb-3 text-xs">
              External images and links are disabled so viewing this message
              does not record an open or a click.
            </p>
            <MailFrame html={html} readOnly title="Stored email content" />
          </TabsPanel>
          <TabsPanel value="html">
            <Source text={html} />
          </TabsPanel>
        </>
      )}
      {text === null ? null : (
        <TabsPanel value="text">
          <Source text={text} />
        </TabsPanel>
      )}
      {content.headers.length > 0 ? (
        <TabsPanel value="headers">
          <Source
            text={content.headers
              .map(([name, value]) => `${name}: ${value}`)
              .join("\n")}
          />
        </TabsPanel>
      ) : null}
    </Tabs>
  );
};

/** Content is an independent read: a failure must not hide delivery attempts or receipts. */
export const MessageContentCard = ({ message }: { message: MessageObject }) => {
  const workspace = useWorkspace();
  const query = useQuery(
    messageContentQuery(workspace, message.id, [
      message.state,
      message.attempts_count,
    ])
  );
  const content = query.data;
  let body: ReactNode = <Skeleton className="h-40" />;
  if (query.isError) {
    body = (
      <Problem
        error={query.error}
        onRetry={() => {
          void query.refetch();
        }}
      />
    );
  } else if (content) {
    body = (
      <>
        {content.truncated ? (
          <p className="text-warning mb-3 text-sm">
            Only part of this message was retained.
          </p>
        ) : null}
        <StoredBody content={content} />
      </>
    );
  }
  return (
    <Card>
      <CardHeader>
        <div className="flex flex-col gap-1">
          <CardTitle>Message content</CardTitle>
          <CardDescription>
            {content?.prepared_at
              ? `Saved for the latest sending attempt on ${formatTimestamp(content.prepared_at)}. Includes the resolved variables and sending links.`
              : "The final campaign email is saved when it is prepared for sending. Any body shown before that is the submitted content."}
          </CardDescription>
        </div>
        <Button
          disabled={query.isFetching}
          onClick={() => {
            void query.refetch();
          }}
          size="s"
          variant="tertiary"
        >
          Refresh
        </Button>
      </CardHeader>
      <CardContent>{body}</CardContent>
    </Card>
  );
};
