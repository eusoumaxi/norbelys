"""Lazy iteration keeps list filters and uses only cursors supplied by the API.

A repeated cursor is rejected instead of creating an infinite request loop. Empty final pages
and missing next cursors end iteration; the SDK never guesses a cursor or loads all pages at
once. Sync and asynchronous generated resources share these helpers.
"""

from collections.abc import AsyncIterator, Awaitable, Callable, Iterator, Mapping
from typing import Any, TypeVar, cast

T = TypeVar("T")


def _next(page: Mapping[str, Any], seen: set[str]) -> str | None:
    meta = page.get("meta", {})
    cursor = meta.get("next_cursor") if meta.get("has_more") else None
    if not cursor:
        return None
    if not isinstance(cursor, str) or cursor in seen:
        raise ValueError("The API returned an invalid or repeated pagination cursor.")
    seen.add(cursor)
    return cursor


def iter_items(load: Callable[[str | None], Mapping[str, Any]]) -> Iterator[T]:
    """Yield one item at a time from a synchronous page loader."""
    cursor = None
    seen: set[str] = set()
    while True:
        page = load(cursor)
        yield from cast("list[T]", page["data"])
        cursor = _next(page, seen)
        if cursor is None:
            return


async def async_iter_items(
    load: Callable[[str | None], Awaitable[Mapping[str, Any]]],
) -> AsyncIterator[T]:
    """Await each page only after the preceding page's items have been consumed."""
    cursor = None
    seen: set[str] = set()
    while True:
        page = await load(cursor)
        for item in page["data"]:
            yield cast("T", item)
        cursor = _next(page, seen)
        if cursor is None:
            return
