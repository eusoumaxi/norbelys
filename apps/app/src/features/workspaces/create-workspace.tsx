import { ArrowDown01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useNavigate } from "@tanstack/react-router";
import { cn } from "cn";
import { useState } from "react";

import { ProblemAlert } from "@/components/problem";
import { Button } from "@/components/ui/button";
import {
  Field,
  FieldDescription,
  FieldError,
  FieldLabel,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupInput,
} from "@/components/ui/input-group";
import { Select } from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { useRefreshMe, useSession } from "@/lib/auth";
import { BROWSER_ZONE, TIME_ZONES } from "@/lib/form";
import { fieldProblems, problemLine } from "@/lib/problem";

/** What the API makes of a name when no slug is given, before its uniqueness suffix. */
const slugFrom = (name: string) =>
  name
    .toLowerCase()
    .normalize("NFKD")
    .replaceAll(/[^a-z0-9]+/gu, "-")
    .replaceAll(/^-+|-+$/gu, "")
    .slice(0, 50);

/**
 * Creates a workspace (`POST /v1/workspaces`) owned by the person, then opens its overview. It
 * asks one thing, the name; its address and its time zone have good defaults and wait under "More
 * options" for whoever wants to choose them.
 */
export const CreateWorkspace = () => {
  const session = useSession();
  const refreshMe = useRefreshMe();
  const navigate = useNavigate();
  const [name, setName] = useState("");
  const [slug, setSlug] = useState("");
  const [timezone, setTimezone] = useState(BROWSER_ZONE);
  const [more, setMore] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const fields = fieldProblems(failure);
  const derived = slugFrom(name);
  // A refused address or time zone must be seen, even when the options were folded away.
  const showMore = more || Boolean(fields.slug || fields.timezone);

  const submit = async () => {
    setBusy(true);
    setFailure(null);
    try {
      const created = await session.createWorkspace({
        name: name.trim(),
        slug: slug.trim() || undefined,
        timezone,
      });
      await refreshMe();
      await navigate({ params: { slug: created.slug }, to: "/w/$slug" });
      return;
    } catch (error) {
      setFailure(error);
    }
    setBusy(false);
  };

  return (
    <form
      className="flex flex-col gap-5"
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      <Field>
        <FieldLabel htmlFor="workspace-name">Company or team name</FieldLabel>
        <Input
          aria-invalid={Boolean(fields.name)}
          autoFocus
          className="h-10"
          id="workspace-name"
          onChange={(event) => setName(event.target.value)}
          placeholder="Acme"
          required
          value={name}
        />
        {fields.name ? (
          <FieldError>{fields.name}</FieldError>
        ) : (
          <FieldDescription>
            Working for several clients? Give each one its own workspace.
          </FieldDescription>
        )}
      </Field>
      <div className="flex flex-col gap-4">
        <button
          aria-expanded={showMore}
          className="text-fg-3 hover:text-fg flex w-fit cursor-pointer items-center gap-1 text-xs font-semibold transition-colors"
          onClick={() => setMore(!showMore)}
          type="button"
        >
          More options
          <HugeiconsIcon
            className={cn(
              "size-3.5 transition-transform",
              showMore && "rotate-180"
            )}
            icon={ArrowDown01Icon}
          />
        </button>
        {showMore ? (
          <>
            <Field>
              <FieldLabel htmlFor="workspace-slug">Address</FieldLabel>
              <InputGroup>
                <InputGroupAddon className="text-fg-4 font-mono text-xs">
                  {window.location.host}/w/
                </InputGroupAddon>
                <InputGroupInput
                  aria-invalid={Boolean(fields.slug)}
                  className="font-mono"
                  id="workspace-slug"
                  onChange={(event) =>
                    setSlug(event.target.value.toLowerCase())
                  }
                  placeholder={derived ? `${derived}-…` : "acme"}
                  value={slug}
                />
              </InputGroup>
              {fields.slug ? (
                <FieldError>{fields.slug}</FieldError>
              ) : (
                <FieldDescription>
                  Made from the name when left empty.
                </FieldDescription>
              )}
            </Field>
            <Field>
              <FieldLabel htmlFor="workspace-timezone">Time zone</FieldLabel>
              <Select
                id="workspace-timezone"
                onChange={setTimezone}
                options={TIME_ZONES.map((zone) => ({
                  label: zone.replaceAll("_", " "),
                  value: zone,
                }))}
                value={timezone}
              />
              {fields.timezone ? (
                <FieldError>{fields.timezone}</FieldError>
              ) : (
                <FieldDescription>
                  Schedules and sending windows use it, unless a person has a
                  time zone of their own.
                </FieldDescription>
              )}
            </Field>
          </>
        ) : null}
      </div>
      {failure && Object.keys(fields).length === 0 ? (
        <ProblemAlert>{problemLine(failure)}</ProblemAlert>
      ) : null}
      <Button
        className="h-10 w-full"
        disabled={busy || !name.trim()}
        type="submit"
        variant="primary"
      >
        {busy ? <Spinner /> : null}
        Create workspace
      </Button>
    </form>
  );
};
