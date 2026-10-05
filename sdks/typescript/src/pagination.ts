import type { RequestOptions } from "./core";

interface Collection<T = unknown> {
  data: readonly T[];
  meta: { has_more: boolean; next_cursor?: string | null };
}

/** Loads one page: the cursor of a following page, and options overriding the list call's. */
type Load<C> = (
  cursor: string | undefined,
  options: RequestOptions | undefined
) => Promise<C>;

/** One page of a list, as the API returned it, plus `nextPage()`. */
export type Page<C extends Collection> = C & {
  /**
   * The following page with the same filters, or `null` after the last one. `options` override
   * those of the `list()` call for this request, such as a fresh `signal`.
   */
  nextPage: (options?: RequestOptions) => Promise<Page<C> | null>;
};

/**
 * The result of a `list()` call. Await it for the first page, or iterate it for every item:
 *
 * ```ts
 * const page = await norbelys.people.list({ q: "acme" });
 * for await (const person of norbelys.people.list({ q: "acme" })) {}
 * ```
 *
 * Iteration fetches one page at a time and keeps the filters; it never loads the whole list.
 */
export class PagePromise<C extends Collection<T>, T = C["data"][number]>
  implements PromiseLike<Page<C>>, AsyncIterable<T>
{
  readonly #load: Load<C>;
  #first: Promise<Page<C>> | undefined;

  constructor(load: Load<C>) {
    this.#load = load;
  }

  // oxlint-disable-next-line unicorn/no-thenable -- awaiting a list resolves to its first page
  then<A = Page<C>, B = never>(
    onFulfilled?: ((page: Page<C>) => A | PromiseLike<A>) | null,
    onRejected?: ((reason: unknown) => B | PromiseLike<B>) | null
  ): Promise<A | B> {
    this.#first ??= this.#page();
    return this.#first.then(onFulfilled, onRejected);
  }

  async *[Symbol.asyncIterator](): AsyncIterator<T> {
    let page: Page<C> | null = await this;
    while (page) {
      yield* page.data;
      // oxlint-disable-next-line no-await-in-loop -- pages are fetched one at a time, on demand
      page = await page.nextPage();
    }
  }

  async #page(cursor?: string, options?: RequestOptions): Promise<Page<C>> {
    const collection = await this.#load(cursor, options);
    const next = collection.meta.has_more ? collection.meta.next_cursor : null;
    return Object.assign(collection, {
      nextPage: (nextOptions?: RequestOptions) =>
        next ? this.#page(next, nextOptions) : Promise.resolve(null),
    });
  }
}
