import {
  Alert02Icon,
  CheckmarkCircle02Icon,
  FileImportIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useQueryClient } from "@tanstack/react-query";
import { cn } from "cn";
import { useState } from "react";
import type { ChangeEvent } from "react";

import { DialogActions } from "@/components/dialog-actions";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button, buttonVariants } from "@/components/ui/button";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Spinner } from "@/components/ui/spinner";
import { Textarea } from "@/components/ui/textarea";
import { readCsv } from "@/features/imports/csv";
import {
  SUPPRESS_PER_CALL,
  fileEntries,
  inCalls,
  sortEntries,
  textEntries,
} from "@/features/suppressions/addresses";
import { suppressionsKey } from "@/features/suppressions/queries";
import { FormField } from "@/lib/form";
import { formatCount, plural } from "@/lib/format";
import { describeProblem } from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";

/** How a run is going, then how it went. */
interface Run {
  total: number;
  /** Addresses whose call has answered. */
  done: number;
  /** Newly suppressed. */
  suppressed: number;
  /** Suppressed before this run. */
  already: number;
  /** Refused by the API as not addresses, each with its reason. */
  failed: { address: string; reason: string }[];
  /** Why a call failed, which ended the run there, and how many addresses were never sent. */
  stopped: { reason: string; unsent: number } | null;
  finished: boolean;
}

/** "foo, bar and 3 more": the first entries of a list, for a line of text. */
const someOf = (entries: readonly string[], shown = 3): string => {
  const head = entries.slice(0, shown).join(", ");
  return entries.length > shown
    ? `${head} and ${formatCount(entries.length - shown)} more`
    : head;
};

/** Adds the addresses of a CSV file to the list, with a line on what it found. */
const FilePicker = ({
  onFound,
}: {
  onFound: (entries: string[], note: string) => void;
}) => {
  const [reading, setReading] = useState(false);
  const take = async (chosen: File | undefined) => {
    if (!chosen) {
      return;
    }
    setReading(true);
    try {
      const read = await readCsv(chosen);
      if (typeof read === "string") {
        onFound([], read);
      } else {
        const entries = fileEntries(read);
        const { addresses, domains, invalid } = sortEntries(entries);
        const skipped = [...domains, ...invalid];
        const added =
          addresses.length > 0
            ? `${plural(addresses.length, "address", "addresses")} from ${read.name} added to the list.`
            : `${read.name} holds no email addresses.`;
        onFound(
          entries,
          skipped.length > 0
            ? `${added} Left out, not addresses: ${someOf(skipped)}.`
            : added
        );
      }
    } catch {
      onFound([], "The file could not be read. Choose it again.");
    }
    setReading(false);
  };
  return (
    <label
      className={cn(
        buttonVariants({ size: "s", variant: "secondary" }),
        "has-[input:focus-visible]:outline-focus relative w-fit has-[input:focus-visible]:outline-1"
      )}
    >
      {reading ? <Spinner /> : <HugeiconsIcon icon={FileImportIcon} />}
      Add from a CSV file
      <input
        accept=".csv,.tsv,.txt,text/csv,text/plain,text/tab-separated-values"
        className="absolute inset-0 size-full cursor-pointer opacity-0"
        onChange={(event: ChangeEvent<HTMLInputElement>) => {
          void take(event.target.files?.[0]);
          event.target.value = "";
        }}
        type="file"
      />
    </label>
  );
};

/** What the list holds, and what of it is left out, under the list. */
const ListSummary = ({ text }: { text: string }) => {
  const found = sortEntries(textEntries(text));
  const { addresses, domains, invalid } = found;
  if (addresses.length === 0 && domains.length === 0 && invalid.length === 0) {
    return null;
  }
  return (
    <div className="flex flex-col gap-1 text-xs">
      <p className="text-fg-2">
        {plural(addresses.length, "address", "addresses")} to suppress.
      </p>
      {domains.length > 0 ? (
        <p className="text-warning">
          Left out, as suppressions are by address, not by domain:{" "}
          {someOf(domains)}.
        </p>
      ) : null}
      {invalid.length > 0 ? (
        <p className="text-warning">
          Left out, as they are not email addresses: {someOf(invalid)}.
        </p>
      ) : null}
    </div>
  );
};

/** How a run went, once it ended; how it is going, while it runs. */
const RunReport = ({ run }: { run: Run }) => {
  if (!run.finished) {
    return (
      <p className="text-fg-2 flex items-center gap-2 text-sm">
        <Spinner className="text-fg-3" />
        Suppressing {plural(run.total, "address", "addresses")}…
        {run.total > SUPPRESS_PER_CALL
          ? ` ${formatCount(run.done)} done.`
          : null}
      </p>
    );
  }
  const lines = [
    run.suppressed > 0
      ? `${plural(run.suppressed, "address", "addresses")} suppressed.`
      : null,
    run.already > 0
      ? `${plural(run.already, run.suppressed > 0 ? "was" : "address was", run.suppressed > 0 ? "were" : "addresses were")} suppressed already.`
      : null,
  ].filter(Boolean);
  return (
    <div className="flex flex-col gap-3">
      {lines.length > 0 ? (
        <p className="text-fg flex items-center gap-2 text-sm">
          <HugeiconsIcon
            className="text-success size-5"
            icon={CheckmarkCircle02Icon}
          />
          {lines.join(" ")}
        </p>
      ) : null}
      {run.failed.length > 0 ? (
        <Alert variant="error">
          <HugeiconsIcon icon={Alert02Icon} />
          <AlertTitle>
            {plural(run.failed.length, "address", "addresses")} not suppressed
          </AlertTitle>
          <AlertDescription>
            <ul className="flex max-h-40 flex-col gap-1 overflow-y-auto">
              {run.failed.map((failure) => (
                <li key={failure.address}>
                  <span className="font-medium">{failure.address}</span>:{" "}
                  {failure.reason}
                </li>
              ))}
            </ul>
          </AlertDescription>
        </Alert>
      ) : null}
      {run.stopped ? (
        <Alert variant="error">
          <HugeiconsIcon icon={Alert02Icon} />
          <AlertTitle>
            {plural(run.stopped.unsent, "address", "addresses")} not sent
          </AlertTitle>
          <AlertDescription>
            {run.stopped.reason} Suppressing the same list again is safe:
            addresses suppressed already are only counted.
          </AlertDescription>
        </Alert>
      ) : null}
    </div>
  );
};

/**
 * Suppresses addresses by hand: one or many, typed or pasted, or added from a CSV file (its email
 * column, or every address of a plain list). The dialog says what suppressing does before
 * anything is sent: it is immediate, no mail of the workspace reaches the address again, and
 * campaigns stop for it. The whole list goes in one call (`suppressions.create` with `emails`,
 * reason `manual`), which the API takes as one decision and answers with how many it suppressed,
 * how many were suppressed already, and each entry it refused, listed with its reason. A list
 * longer than the API takes at once ({@link SUPPRESS_PER_CALL}) goes in consecutive calls of that
 * many, with one progress line; a call that fails ends the run, and the report says how many
 * addresses were not sent. The dialog stays open until the last answer.
 */
export const SuppressDialog = ({
  onOpenChange,
  open,
}: {
  onOpenChange: (open: boolean) => void;
  open: boolean;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const [text, setText] = useState("");
  const [note, setNote] = useState<string | null>(null);
  const [run, setRun] = useState<Run | null>(null);
  const running = run !== null && !run.finished;
  const { addresses } = sortEntries(textEntries(text));

  const suppress = async () => {
    const counts = { already: 0, done: 0, suppressed: 0 };
    const failed: Run["failed"] = [];
    let stopped: Run["stopped"] = null;
    const report = (finished: boolean): Run => ({
      ...counts,
      failed: [...failed],
      finished,
      stopped,
      total: addresses.length,
    });
    setRun(report(false));
    for (const emails of inCalls(addresses)) {
      try {
        // oxlint-disable-next-line no-await-in-loop -- one call after another, each one decision
        const answer = await workspace.api.suppressions.create({ emails });
        counts.already += answer.already;
        counts.done += emails.length;
        counts.suppressed += answer.created;
        failed.push(
          ...answer.invalid.map((entry) => ({
            address: entry.value,
            reason: entry.detail,
          }))
        );
        setRun(report(false));
      } catch (error) {
        stopped = {
          reason: describeProblem(error).detail,
          unsent: addresses.length - counts.done,
        };
        break;
      }
    }
    setRun(report(true));
    void queryClient.invalidateQueries({
      queryKey: suppressionsKey(workspace),
    });
  };

  return (
    <Dialog
      onOpenChange={(next) => {
        if (running) {
          return;
        }
        onOpenChange(next);
      }}
      onOpenChangeComplete={(next) => {
        if (!next) {
          setText("");
          setNote(null);
          setRun(null);
        }
      }}
      open={open}
    >
      <DialogContent className="max-w-[600px]">
        <DialogHeader>
          <DialogTitle>Suppress addresses</DialogTitle>
          <DialogDescription>
            Suppressing is immediate: no campaign, message or API call of this
            workspace reaches these addresses again, and anyone in a running
            campaign stops. You can lift a suppression you add here later.
          </DialogDescription>
        </DialogHeader>
        <DialogBody>
          {run ? (
            <RunReport run={run} />
          ) : (
            <>
              <FormField
                description="One per line, or separated by commas or spaces."
                htmlFor="suppress-addresses"
                label="Email addresses"
              >
                <Textarea
                  autoFocus
                  className="max-h-60 min-h-28 overflow-y-auto"
                  id="suppress-addresses"
                  onChange={(event) => setText(event.target.value)}
                  placeholder={"someone@example.com\nsomeone.else@example.org"}
                  spellCheck={false}
                  value={text}
                />
              </FormField>
              <div className="flex flex-wrap items-center gap-3">
                <FilePicker
                  onFound={(entries, line) => {
                    setNote(line);
                    const found = sortEntries(entries).addresses;
                    if (found.length > 0) {
                      setText((current) =>
                        [current.trim(), ...found].filter(Boolean).join("\n")
                      );
                    }
                  }}
                />
                {note ? (
                  <span className="text-fg-3 text-xs">{note}</span>
                ) : null}
              </div>
              <ListSummary text={text} />
            </>
          )}
        </DialogBody>
        <DialogActions>
          {run?.finished ? null : (
            <Button
              disabled={running || addresses.length === 0}
              onClick={() => {
                void suppress();
              }}
              variant="primary"
            >
              {running ? <Spinner /> : null}
              {addresses.length > 0
                ? `Suppress ${plural(addresses.length, "address", "addresses")}`
                : "Suppress addresses"}
            </Button>
          )}
        </DialogActions>
      </DialogContent>
    </Dialog>
  );
};
