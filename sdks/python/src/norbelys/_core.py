"""One request policy shared by synchronous and asynchronous transports.

Path values are escaped as individual segments and structured query values are encoded as
JSON. Authentication is resolved for every attempt. An idempotency key is generated once per
logical write and survives all retries. Unsafe writes are never automatically retried; GET,
DELETE and operations declaring idempotency can repeat on transport errors, rate limits and
transient server responses. HTTP redirects stay disabled to retain credentials at the API.
"""

import asyncio
import json
import math
import os
import random
import time
import uuid
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from datetime import UTC, datetime
from email.utils import parsedate_to_datetime
from typing import Any
from urllib.parse import quote

import httpx

from .errors import APIConnectionError, APIError, NorbelysError

Token = str | Callable[[], str]
RETRY_STATUSES = frozenset({408, 429, 500, 502, 503, 504})


@dataclass(frozen=True)
class RequestOptions:
    """Overrides for one logical request, reused across its attempts.

    ``idempotency_key`` should be reused when the caller repeats a write. ``if_match`` accepts
    a numeric resource version or an ETag; bare numeric versions are quoted automatically.
    ``timeout`` is the limit in seconds for each HTTP attempt. ``max_retries`` counts retries
    after the first attempt, and applies only to operations safe to repeat.
    """

    idempotency_key: str | None = None
    if_match: int | str | None = None
    timeout: float | None = None
    max_retries: int | None = None
    headers: Mapping[str, str] | None = None


def _body(response: httpx.Response) -> Any:
    """Return JSON when possible, response text otherwise, and None for a bodyless response."""
    if not response.content:
        return None
    try:
        return response.json()
    except ValueError:
        return response.text


def _delay(response: httpx.Response | None, attempt: int) -> float | None:
    """Honor bounded Retry-After values; stop on permanent errors and excessively long waits."""
    if response is not None:
        data = _body(response)
        retryable = response.status_code in RETRY_STATUSES or (
            response.status_code == 409
            and isinstance(data, dict)
            and data.get("code") == "idempotency_in_progress"
        )
        if not retryable:
            return None
        value = response.headers.get("retry-after")
        if value:
            try:
                seconds = float(value)
            except ValueError:
                try:
                    seconds = (parsedate_to_datetime(value) - datetime.now(UTC)).total_seconds()
                except (ValueError, TypeError, OverflowError):
                    seconds = -1
            if math.isfinite(seconds) and seconds >= 0:
                return seconds if seconds <= 60 else None
    return float(min(8.0, 0.5 * 2**attempt) * random.uniform(0.75, 1.0))


class _Policy:
    """Build requests and decide retry limits independently of the transport's scheduling."""

    def __init__(
        self,
        *,
        api_key: str | None = None,
        token: Token | None = None,
        base_url: str = "https://api.norbelys.com",
        timeout: float = 60,
        max_retries: int = 2,
        headers: Mapping[str, str] | None = None,
    ) -> None:
        self.api_key = api_key or (None if token else os.environ.get("NORBELYS_API_KEY"))
        if self.api_key and token:
            raise NorbelysError("Pass api_key or token, not both.")
        if not self.api_key and not token:
            raise NorbelysError("Pass api_key or token, or set NORBELYS_API_KEY.")
        url = httpx.URL(base_url)
        if (
            url.scheme not in {"http", "https"}
            or not url.host
            or url.userinfo
            or url.query
            or url.fragment
        ):
            raise NorbelysError(
                "base_url must be an HTTP origin without credentials, query or fragment."
            )
        if not math.isfinite(timeout) or timeout <= 0 or max_retries < 0:
            raise NorbelysError("timeout must be positive and max_retries nonnegative.")
        self.token = token
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout
        self.max_retries = max_retries
        self.headers = dict(headers or {})

    def _prepare(
        self,
        method: str,
        path: str,
        path_args: Sequence[str],
        *,
        query: Mapping[str, Any] | None,
        body: Any,
        content_type: str | None,
        idempotent: bool,
        options: RequestOptions | None,
    ) -> tuple[str, dict[str, Any], int]:
        options = options or RequestOptions()
        retries = self.max_retries if options.max_retries is None else options.max_retries
        timeout = self.timeout if options.timeout is None else options.timeout
        if retries < 0 or not math.isfinite(timeout) or timeout <= 0:
            raise NorbelysError("timeout must be positive and max_retries nonnegative.")
        for value in path_args:
            if not value or "{" not in path:
                raise NorbelysError("Every path parameter must be supplied exactly once.")
            start = path.index("{")
            end = path.index("}", start) + 1
            path = path[:start] + quote(value, safe="") + path[end:]
        if "{" in path:
            raise NorbelysError("A path parameter is missing.")
        params: list[tuple[str, str]] = []
        for key, value in (query or {}).items():
            for item in value if isinstance(value, list) else [value]:
                if item is not None:
                    rendered = (
                        json.dumps(item, separators=(",", ":"))
                        if isinstance(item, (dict, bool))
                        else str(item)
                    )
                    params.append((key, rendered))
        headers = httpx.Headers({**self.headers, **dict(options.headers or {})})
        headers["accept"] = "application/json"
        if options.if_match is not None:
            version = str(options.if_match).strip()
            headers["if-match"] = f'"{version}"' if version.isdecimal() else version
        if idempotent or options.idempotency_key:
            headers["idempotency-key"] = options.idempotency_key or str(uuid.uuid4())
        kwargs: dict[str, Any] = {
            "params": params,
            "headers": headers,
            "timeout": timeout,
            "follow_redirects": False,
        }
        if body is not None:
            if content_type and content_type != "application/json":
                kwargs["content"] = body
                headers["content-type"] = content_type
            else:
                kwargs["json"] = body
        return (
            self.base_url + path,
            kwargs,
            retries if method in {"GET", "DELETE"} or idempotent else 0,
        )

    def _authorize(self, kwargs: dict[str, Any]) -> None:
        """Refresh a bearer token per attempt; callback failures are propagated without retries."""
        token = self.api_key or (self.token() if callable(self.token) else self.token)
        if not token:
            raise NorbelysError("The token callback returned an empty credential.")
        kwargs["headers"]["authorization"] = f"Bearer {token}"


class SyncCore(_Policy):
    """Execute the shared request policy using a reusable synchronous HTTPX client."""

    def __init__(self, *, http_client: httpx.Client | None = None, **kwargs: Any) -> None:
        super().__init__(**kwargs)
        self.client = http_client or httpx.Client()
        self.owns_client = http_client is None

    def close(self) -> None:
        """Release connections owned by this SDK; externally supplied clients remain open."""
        if self.owns_client:
            self.client.close()

    def request(
        self,
        method: str,
        path: str,
        path_args: Sequence[str],
        *,
        query: Mapping[str, Any] | None = None,
        body: Any = None,
        content_type: str | None = None,
        idempotent: bool = False,
        options: RequestOptions | None = None,
    ) -> Any:
        """Send one logical request, preserving its body and idempotency key across safe retries."""
        url, kwargs, retries = self._prepare(
            method,
            path,
            path_args,
            query=query,
            body=body,
            content_type=content_type,
            idempotent=idempotent,
            options=options,
        )
        attempt = 0
        while True:
            self._authorize(kwargs)
            response = None
            try:
                response = self.client.request(method, url, **kwargs)
            except httpx.TransportError as error:
                if attempt == retries:
                    raise APIConnectionError(error) from error
            if response is not None and response.is_success:
                return _body(response)
            delay = _delay(response, attempt)
            if response is not None and (attempt == retries or delay is None):
                raise APIError(response, _body(response))
            assert delay is not None
            time.sleep(delay)
            attempt += 1


class AsyncCore(_Policy):
    """Execute the same policy without blocking the asyncio event loop; cancellation propagates."""

    def __init__(self, *, http_client: httpx.AsyncClient | None = None, **kwargs: Any) -> None:
        super().__init__(**kwargs)
        self.client = http_client or httpx.AsyncClient()
        self.owns_client = http_client is None

    async def close(self) -> None:
        """Release SDK-owned asynchronous connections."""
        if self.owns_client:
            await self.client.aclose()

    async def request(
        self,
        method: str,
        path: str,
        path_args: Sequence[str],
        *,
        query: Mapping[str, Any] | None = None,
        body: Any = None,
        content_type: str | None = None,
        idempotent: bool = False,
        options: RequestOptions | None = None,
    ) -> Any:
        """Await one logical request with safe retries and a fresh bearer token per attempt."""
        url, kwargs, retries = self._prepare(
            method,
            path,
            path_args,
            query=query,
            body=body,
            content_type=content_type,
            idempotent=idempotent,
            options=options,
        )
        attempt = 0
        while True:
            self._authorize(kwargs)
            response = None
            try:
                response = await self.client.request(method, url, **kwargs)
            except httpx.TransportError as error:
                if attempt == retries:
                    raise APIConnectionError(error) from error
            if response is not None and response.is_success:
                return _body(response)
            delay = _delay(response, attempt)
            if response is not None and (attempt == retries or delay is None):
                raise APIError(response, _body(response))
            assert delay is not None
            await asyncio.sleep(delay)
            attempt += 1
