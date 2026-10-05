import type { PersonObject } from "@norbelys/sdk";

import { Dash } from "@/components/data-table";
import type { Column } from "@/components/data-table";
import { formatName, formatRelative } from "@/lib/format";

/** The columns every list of people shows, by name: who each person is, and when they were added. */
export const PERSON_COLUMNS = {
  added: {
    render: (p) => formatRelative(p.created_at),
    header: "Added",
    id: "created",
  },
  company: {
    render: (p) => p.company || <Dash />,
    header: "Company",
    id: "company",
  },
  email: {
    render: (p) => <span className="text-fg font-bold">{p.email}</span>,
    header: "Email",
    id: "email",
  },
  name: {
    render: (p) => formatName(p) || <Dash />,
    header: "Name",
    id: "name",
  },
} satisfies Record<string, Column<PersonObject>>;
