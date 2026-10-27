import type { Meta } from "@norbelys/sdk";

interface Recipient {
  person_id: string;
  email: string;
  sent: number;
  opened: number;
  clicked: number;
  opens_tracked: boolean;
}

export interface Recipients {
  data: Recipient[];
  meta: Meta;
}

/** CSV cells cannot become spreadsheet formulas when exported addresses start with punctuation. */
export const csvCell = (value: string): string => {
  const safe = /^[=+@\-\t\r\n]/u.test(value) ? `'${value}` : value;
  return `"${safe.replaceAll('"', '""')}"`;
};

/** Read the entire bounded audience before returning a file; errors never yield partial CSV. */
export const collectAudience = async (
  read: (after: string | null) => Promise<Recipients>
): Promise<string> => {
  const rows = ["email,person_id,sent,opened,clicked"];
  const seen = new Set<string>();
  let after: string | null = null;
  do {
    // oxlint-disable-next-line no-await-in-loop -- cursor pages must be fetched in order
    const page = await read(after);
    for (const person of page.data) {
      if (!seen.has(person.person_id)) {
        seen.add(person.person_id);
        rows.push(
          [
            csvCell(person.email),
            csvCell(person.person_id),
            person.sent,
            person.opened,
            person.clicked,
          ].join(",")
        );
      }
    }
    after = page.meta.has_more ? (page.meta.next_cursor ?? null) : null;
    if (page.meta.has_more && !after) {
      throw new Error(
        "The server returned an incomplete page. No file was exported."
      );
    }
    if (seen.size >= 20_000 && after) {
      throw new Error(
        "This export exceeds 20,000 people. Narrow the audience before exporting."
      );
    }
  } while (after);

  return rows.join("\r\n");
};
