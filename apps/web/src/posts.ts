import { getCollection } from "astro:content";
import type { CollectionEntry } from "astro:content";

/** One blog post, as the content collection holds it. */
export type Post = CollectionEntry<"blog">;

/** The published posts, newest first. */
export const getPosts = async (): Promise<Post[]> => {
  const posts = await getCollection("blog", (post) => !post.data.draft);
  return posts.toSorted(
    (a, b) => b.data.date.valueOf() - a.data.date.valueOf()
  );
};

/** Minutes to read a post's Markdown at an unhurried pace, never less than one. */
export const readingMinutes = (post: Post): number => {
  const words = (post.body ?? "").split(/\s+/u).filter(Boolean).length;
  return Math.max(1, Math.round(words / 230));
};

const DATE = new Intl.DateTimeFormat("en-US", {
  day: "numeric",
  month: "short",
  timeZone: "UTC",
  year: "numeric",
});

/** A post's date the way a US reader writes it: "Oct 5, 2026". */
export const formatDate = (date: Date): string => DATE.format(date);

/** Where a post lives. */
export const postUrl = (post: Post): string => `/blog/${post.id}/`;
