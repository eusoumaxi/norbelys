import { SettingsPanel } from "@/components/settings-layout";
import { Checkbox } from "@/components/ui/checkbox";
import { FieldDescription, FieldError } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select } from "@/components/ui/select";
import type { SelectOption } from "@/components/ui/select";
import type { SettingsDraft } from "@/features/campaigns/settings/settings-draft";
import { BROWSER_ZONE, FormField, SwitchField, TIME_ZONES } from "@/lib/form";
import { WEEKDAYS, zoneName } from "@/lib/format";
import { problemAt } from "@/lib/problem";

export interface PanelProps {
  draft: SettingsDraft;
  errors: Readonly<Record<string, string>>;
  set: (patch: Partial<SettingsDraft>) => void;
}

const pad = (n: number) => String(n).padStart(2, "0");

/** Every quarter hour of a day, `00:00` to `23:45`: the window's choices (the API takes 5-minute marks). */
const QUARTERS = Array.from(
  { length: 96 },
  (_, i) => `${pad(Math.floor(i / 4))}:${pad((i % 4) * 15)}`
);

/** The hours a window edge can take, keeping a value set elsewhere (`09:05`) that is no quarter. */
const hourOptions = (value: string, end: boolean): SelectOption[] => {
  const hours = end ? [...QUARTERS.slice(1), "24:00"] : QUARTERS;
  const all =
    value && !hours.includes(value)
      ? [...hours, value].toSorted((a, b) => a.localeCompare(b))
      : hours;
  return all.map((hour) => ({
    label: hour === "24:00" ? "Midnight" : hour,
    value: hour,
  }));
};

/** The time zones, as people read them (`America/New York`), with the campaign's own kept. */
const zoneOptions = (value: string): SelectOption[] =>
  (TIME_ZONES.includes(value) ? TIME_ZONES : [value, ...TIME_ZONES]).map(
    (zone) => ({ label: zone.replaceAll("_", " "), value: zone })
  );

/** The days and hours of the send window, shown while the campaign keeps one. */
const WindowFields = ({ draft, errors, set }: PanelProps) => (
  <div className="border-line flex flex-col gap-4 rounded-sm border p-4">
    <fieldset className="flex flex-col gap-1.5">
      <legend className="text-fg-2 mb-1.5 text-sm font-semibold">Days</legend>
      <div className="flex flex-wrap gap-x-5 gap-y-2">
        {WEEKDAYS.map((day) => (
          <Label className="text-fg gap-2 font-normal" key={day.value}>
            <Checkbox
              checked={draft.days.includes(day.value)}
              onCheckedChange={(checked) =>
                set({
                  days: checked
                    ? [...draft.days, day.value]
                    : draft.days.filter((d) => d !== day.value),
                })
              }
            />
            {day.label}
          </Label>
        ))}
      </div>
    </fieldset>
    <div className="flex flex-wrap gap-4">
      <FormField className="w-36" htmlFor="window-start" label="From">
        <Select
          id="window-start"
          onChange={(value) => set({ start: value })}
          options={hourOptions(draft.start, false)}
          value={draft.start}
        />
      </FormField>
      <FormField className="w-36" htmlFor="window-end" label="Until">
        <Select
          id="window-end"
          onChange={(value) => set({ end: value })}
          options={hourOptions(draft.end, true)}
          value={draft.end}
        />
      </FormField>
    </div>
    {problemAt(errors, "schedule.send_window") ? (
      <FieldError>{problemAt(errors, "schedule.send_window")}</FieldError>
    ) : (
      <FieldDescription>
        {zoneName(draft.timezone || "UTC")}, the campaign&apos;s time zone.
      </FieldDescription>
    )}
  </div>
);

/**
 * When the campaign sends: the days and hours its emails may go out (read in its time zone), and
 * a moment before which nothing goes out. Each mailbox's own hours and daily limit still apply.
 */
export const SchedulePanel = (props: PanelProps) => {
  const { draft, errors, set } = props;
  return (
    <SettingsPanel
      description="Emails go out only at these times. Each mailbox's own hours and daily limit apply too."
      title="When it sends"
    >
      <SwitchField
        checked={draft.windowed}
        id="schedule-windowed"
        label="Only send on certain days and hours"
        onChange={(checked) => set({ windowed: checked })}
      />
      {draft.windowed ? (
        <WindowFields {...props} />
      ) : (
        <p className="text-fg-3 text-xs">
          Any day, any hour, as each mailbox allows.
        </p>
      )}
      <div className="grid gap-4 sm:grid-cols-2">
        <FormField
          description="The hours above are read in it."
          htmlFor="schedule-timezone"
          label="Time zone"
          problem={problemAt(errors, "schedule.timezone")}
        >
          <Select
            id="schedule-timezone"
            onChange={(value) => set({ timezone: value })}
            options={zoneOptions(draft.timezone)}
            value={draft.timezone}
          />
        </FormField>
        <FormField
          description={`In your time (${zoneName(BROWSER_ZONE)}). Empty: as soon as it starts.`}
          htmlFor="schedule-start"
          label="Not before"
          optional
          problem={problemAt(errors, "schedule.start_at")}
        >
          <Input
            aria-invalid={Boolean(problemAt(errors, "schedule.start_at"))}
            id="schedule-start"
            onChange={(event) => set({ start_at: event.target.value })}
            type="datetime-local"
            value={draft.start_at}
          />
        </FormField>
      </div>
    </SettingsPanel>
  );
};
