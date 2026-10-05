import { useId, useState } from "react";

import { Button } from "@/components/ui/button";
import { FieldError } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Segmented } from "@/components/ui/segmented";
import { rangeLabel, rangeProblem, recentRange } from "@/lib/date-range";
import type { DateRange } from "@/lib/date-range";
import { FormField } from "@/lib/form";

const PRESETS = ["7", "30", "90"] as const;
type Preset = (typeof PRESETS)[number] | "custom";

/** Presets write dates; Custom edits both ends together before changing the report. */
export const DateRangeControl = ({
  range,
  onChange,
  maxDays,
}: {
  range: DateRange;
  maxDays?: number;
  onChange: (range: DateRange) => void;
}) => {
  const id = useId();
  const preset =
    PRESETS.find((days) => {
      const recent = recentRange(Number(days));
      return recent.from === range.from && recent.to === range.to;
    }) ?? "custom";
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(range);
  const problem = rangeProblem(draft, maxDays);
  return (
    <div className="flex min-w-0 flex-col gap-3">
      <Segmented<Preset>
        className="flex-wrap"
        label="Date range"
        onChange={(value) => {
          setEditing(value === "custom");
          if (value !== "custom") {
            onChange(recentRange(Number(value)));
          }
        }}
        options={[
          { label: "7 days", value: "7" },
          { label: "30 days", value: "30" },
          { label: "90 days", value: "90" },
          { label: "Custom", value: "custom" },
        ]}
        value={editing ? "custom" : preset}
      />
      <p className="text-fg-3 text-xs">{rangeLabel(range)}</p>
      {editing || preset === "custom" ? (
        <form
          className="flex flex-col gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            if (!problem) {
              onChange(draft);
            }
          }}
        >
          <div className="flex flex-wrap items-end gap-3">
            <FormField htmlFor={`${id}-from`} label="From (UTC)">
              <Input
                id={`${id}-from`}
                type="date"
                required
                value={draft.from}
                onChange={(event) =>
                  setDraft((current) => ({
                    ...current,
                    from: event.target.value,
                  }))
                }
              />
            </FormField>
            <FormField htmlFor={`${id}-to`} label="To (UTC)">
              <Input
                id={`${id}-to`}
                type="date"
                required
                min={draft.from}
                value={draft.to}
                onChange={(event) =>
                  setDraft((current) => ({
                    ...current,
                    to: event.target.value,
                  }))
                }
              />
            </FormField>
            <Button
              disabled={Boolean(problem)}
              type="submit"
              variant="secondary"
            >
              Apply dates
            </Button>
          </div>
          {problem ? <FieldError>{problem}</FieldError> : null}
        </form>
      ) : null}
    </div>
  );
};
