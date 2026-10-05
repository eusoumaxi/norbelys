import type { FieldObject } from "@norbelys/sdk";

import { keyFromLabel } from "@/features/people/fields";

/**
 * The CSV files people bring, read in the browser, and the file an import uploads.
 *
 * A chosen file is read whole before anything is sent: its text (UTF-8, or Windows-1252 as Excel
 * writes it on Windows), its separator (a comma, the semicolon spreadsheets write where the comma
 * is the decimal mark, or a tab), its header and its rows. Each column then gets a `Target`, first
 * guessed from its header (the common spellings of the person's own details, in a few languages,
 * and the workspace's custom fields by key or label) and, for the email, from its values. The
 * person corrects what is wrong, and the dashboard uploads a new file holding only the imported
 * columns, each named as the API reads it (`email`, `given_name`, `family_name`, `company`,
 * `fields.<key>`), comma-separated UTF-8. The API reads nothing else, so the person never edits
 * their file by hand.
 *
 * Rows keep their order, so the row numbers an import reports (the header is row 1) point at the
 * same records as in the person's file, blank lines aside: they are skipped here, so a row after
 * one is numbered one less than in the spreadsheet.
 */

/** The largest body `imports.create` accepts. */
export const IMPORT_BYTES_MAX = 16 * 1024 * 1024;

/**
 * The largest file the browser reads. Columns left out never reach the upload, so a wide export
 * may be larger than what is sent; the upload itself is checked against `IMPORT_BYTES_MAX`.
 */
const READ_BYTES_MAX = 64 * 1024 * 1024;

/** A file as read: its name, its header (trimmed) and its data rows, as long as each was written. */
export interface CsvFile {
  name: string;
  header: string[];
  rows: string[][];
}

/** What a spreadsheet application saves natively, which is not CSV text. */
const SPREADSHEET_NAME = /\.(?:xlsx|xlsm|xls|numbers|ods)$/iu;

/**
 * Whether bytes are a spreadsheet's own format: a ZIP archive (`.xlsx`, `.numbers`, `.ods`) or
 * an OLE compound file (`.xls`), recognised by their first bytes whatever the file is named.
 */
const isSpreadsheet = (bytes: ArrayBuffer): boolean => {
  const head = [...new Uint8Array(bytes.slice(0, 4))];
  const zip = [0x50, 0x4b, 0x03, 0x04];
  const ole = [0xd0, 0xcf, 0x11, 0xe0];
  return [zip, ole].some((magic) =>
    magic.every((byte, index) => head[index] === byte)
  );
};

/** What to do with a spreadsheet, for people who never exported one. */
const SPREADSHEET_PROBLEM =
  "This is a spreadsheet file, not CSV. In Excel, Numbers or Google Sheets, save or download it as CSV, then choose that file.";

/**
 * A file's text: UTF-8 (a leading byte order mark dropped), or Windows-1252 when the bytes are
 * not UTF-8, which is how Excel on Windows saves "CSV" and how accented names would otherwise
 * arrive garbled.
 */
const decodeText = (bytes: ArrayBuffer): string => {
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    return new TextDecoder("windows-1252").decode(bytes);
  }
};

/** The separators spreadsheets write between cells. */
const SEPARATORS = [",", ";", "\t"] as const;
type Separator = (typeof SEPARATORS)[number];

/**
 * A file's separator: the one its first line holds most often outside quotes, a comma when the
 * line holds none (a file of one column).
 */
const detectSeparator = (text: string): Separator => {
  const counts = new Map<string, number>(SEPARATORS.map((sep) => [sep, 0]));
  let quoted = false;
  for (const char of text) {
    if (char === '"') {
      quoted = !quoted;
    } else if (!quoted && (char === "\n" || char === "\r")) {
      break;
    } else if (!quoted && counts.has(char)) {
      counts.set(char, (counts.get(char) ?? 0) + 1);
    }
  }
  let best: Separator = ",";
  for (const sep of SEPARATORS) {
    if ((counts.get(sep) ?? 0) > (counts.get(best) ?? 0)) {
      best = sep;
    }
  }
  return best;
};

/** The index of the next separator or line break at or after `from`, or the end of the text. */
const nextStop = (stop: RegExp, text: string, from: number): number => {
  stop.lastIndex = from;
  return stop.exec(text)?.index ?? text.length;
};

/**
 * A quoted cell starting at `from` (on its opening quote): its value, with `""` read as one
 * quote, and where reading continues. Anything between the closing quote and the next separator
 * is kept, as spreadsheets do; an unterminated quote runs to the end of the text.
 */
const quotedCell = (
  stop: RegExp,
  text: string,
  from: number
): [string, number] => {
  let value = "";
  let index = from + 1;
  for (;;) {
    const quote = text.indexOf('"', index);
    if (quote === -1) {
      return [value + text.slice(index), text.length];
    }
    value += text.slice(index, quote);
    if (text[quote + 1] === '"') {
      value += '"';
      index = quote + 2;
    } else {
      const end = nextStop(stop, text, quote + 1);
      return [value + text.slice(quote + 1, end), end];
    }
  }
};

/**
 * Parses CSV text (RFC 4180, with `separator` between cells): quoted cells may hold separators,
 * quotes and line breaks; lines end with LF, CRLF or CR; a leading byte order mark is dropped and
 * empty lines are skipped.
 */
const parseCsv = (text: string, separator: Separator): string[][] => {
  const stop = new RegExp(`[${separator}\\r\\n]`, "gu");
  const rows: string[][] = [];
  let row: string[] = [];
  let index = text.codePointAt(0) === 0xfe_ff ? 1 : 0;
  while (index <= text.length) {
    let cell = "";
    if (text[index] === '"') {
      [cell, index] = quotedCell(stop, text, index);
    } else {
      const end = nextStop(stop, text, index);
      cell = text.slice(index, end);
      index = end;
    }
    row.push(cell);
    if (text[index] === separator) {
      index += 1;
    } else {
      if (row.length > 1 || row[0] !== "") {
        rows.push(row);
      }
      row = [];
      index += text.startsWith("\r\n", index) ? 2 : 1;
    }
  }
  return rows;
};

/** A cell as CSV writes it: quoted when it holds a comma, a quote, a line break or edge spaces. */
const quoteCell = (cell: string): string =>
  /[",\r\n]/u.test(cell) || cell !== cell.trim()
    ? `"${cell.replaceAll('"', '""')}"`
    : cell;

/** Rows as comma-separated CSV text, CRLF line ends. */
const writeCsv = (rows: string[][]): string =>
  `${rows.map((row) => row.map(quoteCell).join(",")).join("\r\n")}\r\n`;

/**
 * Reads a chosen file as CSV, or says in words why it can't be: a spreadsheet's own format, too
 * large, empty, or a header without rows.
 */
export const readCsv = async (file: File): Promise<CsvFile | string> => {
  if (SPREADSHEET_NAME.test(file.name)) {
    return SPREADSHEET_PROBLEM;
  }
  if (file.size > READ_BYTES_MAX) {
    return "The file is larger than 64 MB. Split it into smaller files and import each one.";
  }
  const bytes = await file.arrayBuffer();
  if (isSpreadsheet(bytes)) {
    return SPREADSHEET_PROBLEM;
  }
  const text = decodeText(bytes);
  const [header, ...rows] = parseCsv(text, detectSeparator(text));
  if (!header || header.every((name) => !name.trim())) {
    return "The file is empty. Its first row must name the columns.";
  }
  if (rows.length === 0) {
    return "The file has a header but no rows.";
  }
  return { header: header.map((name) => name.trim()), name: file.name, rows };
};

/**
 * What a column is imported as: one of the person's own details (`email`, `given_name`,
 * `family_name`, `company`), an existing custom field (`fields.<key>`), a new custom field named
 * after the column (`NEW_FIELD`), or nothing (`SKIP`).
 */
export type Target = string;

/** A column left out of the upload. */
export const SKIP: Target = "skip";

/** A column imported into a new text field, created with the column's name just before upload. */
export const NEW_FIELD: Target = "new";

/** The person's own details a column can hold, in the order a picker offers them. */
export const ATTRIBUTES: { label: string; value: Target }[] = [
  { label: "Email", value: "email" },
  { label: "First name", value: "given_name" },
  { label: "Last name", value: "family_name" },
  { label: "Company", value: "company" },
];

/**
 * A name as headers are compared: accents dropped, lowercase, and every run of anything but
 * letters and digits as one `_` (`E-mail Address` and `email_address` are one name).
 */
const normalize = (name: string): string =>
  name
    .normalize("NFKD")
    .replaceAll(/\p{M}/gu, "")
    .toLowerCase()
    .replaceAll(/[^a-z0-9]+/gu, "_")
    .replaceAll(/^_+|_+$/gu, "");

/**
 * The header names read as each of the person's own details, normalized: the API's spellings,
 * what spreadsheets and CRMs export, and the same words in Spanish, French, German, Italian and
 * Portuguese.
 */
const SYNONYMS = new Map<string, Target>(
  Object.entries({
    company: [
      "account_name",
      "azienda",
      "business",
      "business_name",
      "company",
      "company_name",
      "companyname",
      "empresa",
      "employer",
      "entreprise",
      "firma",
      "org",
      "organisation",
      "organisation_name",
      "organization",
      "organization_name",
      "societe",
      "unternehmen",
    ],
    email: [
      "adresse_email",
      "business_email",
      "contact_email",
      "correo",
      "correo_electronico",
      "courriel",
      "e_mail",
      "e_mail_address",
      "email",
      "email_1",
      "email_address",
      "emailaddress",
      "mail",
      "primary_email",
      "work_email",
    ],
    family_name: [
      "apellido",
      "apellidos",
      "cognome",
      "family_name",
      "familyname",
      "last",
      "last_name",
      "lastname",
      "nachname",
      "nom",
      "nom_de_famille",
      "sobrenome",
      "surname",
    ],
    given_name: [
      "first",
      "first_name",
      "firstname",
      "forename",
      "given_name",
      "givenname",
      "nombre",
      "nome",
      "prenom",
      "primeiro_nome",
      "vorname",
    ],
  }).flatMap(([target, names]) =>
    names.map((name): [string, Target] => [name, target])
  )
);

/**
 * What a header names: one of the person's details, or a custom field whose key or label reads
 * the same (underscores aside: `Job title` is `job_title` and `jobtitle`), or nothing. A
 * `fields.` prefix, the API's own way to name a field, is read through.
 */
const targetOfHeader = (
  header: string,
  fields: readonly FieldObject[]
): Target => {
  const name = normalize(header);
  const bare = name.startsWith("fields_") ? name.slice("fields_".length) : name;
  const attribute = SYNONYMS.get(bare);
  if (attribute) {
    return attribute;
  }
  const compact = bare.replaceAll("_", "");
  const field = fields.find((candidate) =>
    [candidate.key, candidate.label]
      .map(normalize)
      .some((known) => known === bare || known.replaceAll("_", "") === compact)
  );
  return bare && field ? `fields.${field.key}` : SKIP;
};

/** An address as a cell or a list holds it: one `@`, a dotted domain, no spaces or brackets. */
export const ADDRESS =
  /^[^\s@<>()[\],;:"]+@[^\s@<>()[\],;:"]+\.[^\s@<>()[\],;:"]+$/u;

/** How many of a column's first values are looked at to recognise addresses. */
const SAMPLE_ROWS = 50;

/**
 * The column whose values are addresses, for a file whose header names no email column: among
 * the columns still left out, the one with the most addresses in its first rows, provided at
 * least four in five of its filled cells there are addresses.
 */
const addressColumn = (
  file: CsvFile,
  targets: readonly Target[]
): number | undefined => {
  const sample = file.rows.slice(0, SAMPLE_ROWS);
  let best: { column: number; hits: number } | undefined;
  for (const [column, target] of targets.entries()) {
    const values = sample
      .map((row) => row[column]?.trim() ?? "")
      .filter(Boolean);
    const hits = values.filter((value) => ADDRESS.test(value)).length;
    if (
      target === SKIP &&
      hits > 0 &&
      hits * 5 >= values.length * 4 &&
      hits > (best?.hits ?? 0)
    ) {
      best = { column, hits };
    }
  }
  return best?.column;
};

/**
 * A first target for every column of `file`: what its header names, the first column winning
 * when two name the same thing; then, when no header names the email, the column whose values
 * are addresses. Columns that name nothing are left out: creating a field is the person's choice.
 */
export const guessTargets = (
  file: CsvFile,
  fields: readonly FieldObject[]
): Target[] => {
  const used = new Set<Target>();
  const targets = file.header.map((header) => {
    const target = targetOfHeader(header, fields);
    if (target === SKIP || used.has(target)) {
      return SKIP;
    }
    used.add(target);
    return target;
  });
  if (!used.has("email")) {
    const column = addressColumn(file, targets);
    if (column !== undefined) {
      targets[column] = "email";
    }
  }
  return targets;
};

/** A column's first filled values, to recognise it by. */
export const examples = (
  rows: readonly string[][],
  column: number,
  count = 3
): string[] => {
  const found: string[] = [];
  for (const row of rows) {
    const value = row[column]?.trim();
    if (value) {
      found.push(value);
      if (found.length === count) {
        break;
      }
    }
  }
  return found;
};

/** The targets two or more columns share (a new field is the column's own, never shared). */
export const repeatedTargets = (targets: readonly Target[]): Target[] => {
  const seen = new Set<Target>();
  const repeated = new Set<Target>();
  for (const target of targets) {
    if (target !== SKIP && target !== NEW_FIELD && seen.has(target)) {
      repeated.add(target);
    }
    seen.add(target);
  }
  return [...repeated];
};

/**
 * Keys the API refuses for a custom field: the person's own details and the names imports and
 * segments read as them.
 */
const RESERVED_KEYS = new Set([
  "company",
  "company_name",
  "created_at",
  "e_mail",
  "email",
  "email_address",
  "email_domain",
  "family_name",
  "first_name",
  "firstname",
  "given_name",
  "id",
  "last_name",
  "lastname",
  "organisation",
  "organization",
  "surname",
  "updated_at",
]);

/**
 * The key of a new field named after a column: suggested from its name as the field form does,
 * then made unique among `taken` with `_2`, `_3`… (`column` when the name has no letter to start
 * a key with).
 */
export const newFieldKey = (
  name: string,
  taken: ReadonlySet<string>
): string => {
  const base = keyFromLabel(name) || "column";
  const free = (key: string) => !(taken.has(key) || RESERVED_KEYS.has(key));
  let key = base;
  for (let suffix = 2; !free(key); suffix += 1) {
    key = `${base.slice(0, 60)}_${suffix}`;
  }
  return key;
};

/** The label of a new field named after the column at `index`: its name, at most 100 characters. */
export const newFieldLabel = (name: string, index: number): string =>
  name.trim().slice(0, 100) || `Column ${index + 1}`;

/** One imported column: where it is in the file, and the header the API reads it by. */
export interface UploadColumn {
  index: number;
  name: string;
}

/** The file to upload: the imported columns only, under the names the API reads, rows in order. */
export const uploadCsv = (
  rows: readonly string[][],
  columns: readonly UploadColumn[]
): string =>
  writeCsv([
    columns.map((column) => column.name),
    ...rows.map((row) => columns.map((column) => row[column.index] ?? "")),
  ]);

/** The size of text once encoded as UTF-8, as the API counts a body. */
export const byteLength = (text: string): number =>
  new TextEncoder().encode(text).length;
