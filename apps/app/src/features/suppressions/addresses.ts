import { ADDRESS, guessTargets } from "@/features/imports/csv";
import type { CsvFile } from "@/features/imports/csv";

/**
 * The addresses a person gives to suppress, typed, pasted or read from a CSV file, sorted into
 * what `suppressions.create` can take and what it cannot. The API suppresses a list of addresses
 * in one call (`emails`), and never a whole domain, so a domain given on its own is told apart
 * from a mistyped address: each needs its own words.
 */

/**
 * The most addresses one call suppresses: the API's own bound on `emails`. A longer list goes in
 * consecutive calls of this many ({@link inCalls}).
 */
export const SUPPRESS_PER_CALL = 1000;

/** What a person gave, sorted. */
interface Found {
  /** Addresses, each once (compared ignoring case, as the API keys them), in the order given. */
  addresses: string[];
  /** Domains given on their own (`example.com`, `@example.com`). */
  domains: string[];
  /** Entries with an `@` that are not addresses. */
  invalid: string[];
}

/** What wraps an address in a list or a sentence: quotes, brackets, a closing full stop. */
const WRAPPING = /^[<(["']+|[>)\]"'.]+$/gu;

/** A domain written alone: labels joined by dots, perhaps after an `@`. */
const DOMAIN = /^@?[a-z0-9-]+(?:\.[a-z0-9-]+)+$/iu;

/**
 * Sorts entries (the words of pasted text, the cells of a file) into addresses, domains and
 * mistakes. Other words, such as the name in `Ada Lovelace <ada@example.com>`, are left out.
 */
export const sortEntries = (entries: Iterable<string>): Found => {
  const seen = new Set<string>();
  const found: Found = { addresses: [], domains: [], invalid: [] };
  for (const raw of entries) {
    const entry = raw.trim().replaceAll(WRAPPING, "");
    if (ADDRESS.test(entry)) {
      const key = entry.toLowerCase();
      if (!seen.has(key)) {
        seen.add(key);
        found.addresses.push(entry);
      }
    } else if (DOMAIN.test(entry)) {
      found.domains.push(entry.replace(/^@/u, ""));
    } else if (entry.includes("@")) {
      found.invalid.push(entry);
    }
  }
  return found;
};

/** The entries of pasted text: what lies between spaces, commas, semicolons and line breaks. */
export const textEntries = (text: string): string[] => text.split(/[\s,;]+/u);

/**
 * The entries of a CSV file: the cells of its email column (named by its header, or recognised
 * by its values); every cell of a file without one, or of a plain list with no header row.
 */
export const fileEntries = (file: CsvFile): string[] => {
  const column = guessTargets(file, []).indexOf("email");
  const headed = column !== -1 && !ADDRESS.test(file.header[column] ?? "");
  return headed
    ? file.rows.map((row) => row[column] ?? "")
    : [file.header, ...file.rows].flat();
};

/** The addresses as the calls that suppress them: in order, at most {@link SUPPRESS_PER_CALL} each. */
export const inCalls = (addresses: readonly string[]): string[][] =>
  Array.from(
    { length: Math.ceil(addresses.length / SUPPRESS_PER_CALL) },
    (_, call) =>
      addresses.slice(call * SUPPRESS_PER_CALL, (call + 1) * SUPPRESS_PER_CALL)
  );
