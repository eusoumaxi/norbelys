import { getCollection } from "astro:content";
import type { CollectionEntry } from "astro:content";

/** One legal document, as the content collection holds it. */
export type LegalDocument = CollectionEntry<"legal">;

/** One version in a document's record. */
export type Revision = LegalDocument["data"]["revisions"][number];

/** The binder's front page. */
export const LEGAL = "/legal";

/** Every legal document, in the binder's order. */
export const getDocuments = async (): Promise<LegalDocument[]> => {
  const documents = await getCollection("legal");
  return documents.toSorted((a, b) => a.data.order - b.data.order);
};

/** Where a document lives. */
export const documentUrl = (document: LegalDocument): string =>
  `${LEGAL}/${document.id}`;

/** The version on the page: the newest entry in the document's record. */
export const latest = (document: LegalDocument): Revision => {
  const [revision] = document.data.revisions;
  if (!revision) {
    throw new Error(`${document.id} has no revision`);
  }
  return revision;
};

/** Minutes to read a document closely: legal text goes slower than a blog post. */
export const readingMinutes = (document: LegalDocument): number => {
  const words = (document.body ?? "").split(/\s+/u).filter(Boolean).length;
  return Math.max(1, Math.round(words / 200));
};

const DATE = new Intl.DateTimeFormat("en-US", {
  dateStyle: "long",
  timeZone: "UTC",
});

/** A date the way a contract writes it: "October 5, 2026". */
export const formatLongDate = (date: Date): string => DATE.format(date);

/** A clause's number and title, from its heading's text ("4 Your data"). */
export const splitHeading = (
  text: string
): { number: string; title: string } => {
  const match = /^(?<number>\d+(?:\.\d+)*)\s+(?<title>.*)$/u.exec(text);
  return {
    number: match?.groups?.number ?? "",
    title: match?.groups?.title ?? text,
  };
};
