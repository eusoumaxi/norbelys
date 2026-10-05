import type { APIRoute } from "astro";

import { getPosts, postUrl } from "../../posts";

/** Text made safe to sit inside an XML element. */
const escape = (text: string): string =>
  text
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");

/** The blog as an RSS feed, for readers who'd rather not check back. */
export const GET: APIRoute = async ({ site }) => {
  const base = site ?? new URL("https://norbelys.com");
  const posts = await getPosts();
  const items = posts.map((post) => {
    const link = new URL(postUrl(post), base).href;
    return [
      "<item>",
      `<title>${escape(post.data.title)}</title>`,
      `<link>${link}</link>`,
      `<guid isPermaLink="true">${link}</guid>`,
      `<description>${escape(post.data.description)}</description>`,
      `<category>${escape(post.data.tag)}</category>`,
      `<pubDate>${post.data.date.toUTCString()}</pubDate>`,
      "</item>",
    ].join("");
  });
  const feed = [
    '<?xml version="1.0" encoding="UTF-8"?>',
    '<rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom">',
    "<channel>",
    "<title>Norbelys blog</title>",
    `<link>${new URL("/blog/", base).href}</link>`,
    `<atom:link href="${new URL("/blog/rss.xml", base).href}" rel="self" type="application/rss+xml"/>`,
    "<description>Playbooks, templates and straight answers about cold email.</description>",
    "<language>en-us</language>",
    ...items,
    "</channel>",
    "</rss>",
  ].join("");
  return new Response(feed, {
    headers: { "Content-Type": "application/rss+xml; charset=utf-8" },
  });
};
