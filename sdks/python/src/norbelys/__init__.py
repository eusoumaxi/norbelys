"""Typed clients for the Norbelys public HTTP API.

Use ``with Norbelys(api_key=...) as client`` or ``async with AsyncNorbelys(...)`` to release
owned connections. Leaving api_key unset reads NORBELYS_API_KEY. Resource methods retain API
field names and return ordinary typed dictionaries; ``client.people.iter()`` fetches pages
lazily. Pass a token string or a synchronous token callback for OAuth-based authentication.
"""

from collections.abc import Mapping
from typing import Self

import httpx

from ._core import AsyncCore, RequestOptions, SyncCore, Token
from ._generated import models
from ._generated.resources import AsyncNorbelysResources, NorbelysResources
from .errors import APIConnectionError, APIError, NorbelysError

__all__ = [
    "APIConnectionError",
    "APIError",
    "AsyncNorbelys",
    "Norbelys",
    "NorbelysError",
    "RequestOptions",
    "models",
]
__version__ = "0.2.0"  # x-release-please-version


class Norbelys(NorbelysResources):
    """Synchronous client; a supplied HTTPX client stays owned by the caller."""

    def __init__(
        self,
        *,
        api_key: str | None = None,
        token: Token | None = None,
        base_url: str = "https://api.norbelys.com",
        timeout: float = 60,
        max_retries: int = 2,
        headers: Mapping[str, str] | None = None,
        http_client: httpx.Client | None = None,
    ) -> None:
        super().__init__(
            SyncCore(
                api_key=api_key,
                token=token,
                base_url=base_url,
                timeout=timeout,
                max_retries=max_retries,
                headers=headers,
                http_client=http_client,
            )
        )

    def close(self) -> None:
        """Close SDK-owned connections; no resource calls may follow this operation."""
        self._core.close()

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


class AsyncNorbelys(AsyncNorbelysResources):
    """Asyncio client with the same typed resources and request policy as the sync client."""

    def __init__(
        self,
        *,
        api_key: str | None = None,
        token: Token | None = None,
        base_url: str = "https://api.norbelys.com",
        timeout: float = 60,
        max_retries: int = 2,
        headers: Mapping[str, str] | None = None,
        http_client: httpx.AsyncClient | None = None,
    ) -> None:
        super().__init__(
            AsyncCore(
                api_key=api_key,
                token=token,
                base_url=base_url,
                timeout=timeout,
                max_retries=max_retries,
                headers=headers,
                http_client=http_client,
            )
        )

    async def close(self) -> None:
        """Release SDK-owned connections without blocking the event loop."""
        await self._core.close()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *_: object) -> None:
        await self.close()
