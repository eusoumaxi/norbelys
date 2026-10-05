"""Errors preserve the API's machine code, request id and response for callers to inspect.

Transport failures are distinct from HTTP problems. A caller can decide whether to retry a
business operation without parsing a human message. Unknown problem codes remain usable when
the server adds a code before a client update is installed.
"""

from typing import Any

import httpx


class NorbelysError(Exception):
    """Base class of errors raised by the client."""


class APIError(NorbelysError):
    """An unsuccessful HTTP response, including non-JSON proxy errors.

    ``body`` is the parsed JSON or original response text. ``code`` identifies an RFC 9457
    problem; ``request_id`` correlates the failed request with server logs. The response and
    headers are retained, but credentials are never added to the error message.
    """

    def __init__(self, response: httpx.Response, body: Any) -> None:
        self.status = response.status_code
        self.body = body
        self.response = response
        self.headers = response.headers
        problem = body if isinstance(body, dict) else {}
        self.code: str | None = problem.get("code")
        self.detail: str | None = problem.get("detail")
        self.title: str | None = problem.get("title")
        self.request_id: str | None = problem.get("request_id") or response.headers.get(
            "x-request-id"
        )
        self.errors: list[dict[str, Any]] = problem.get("errors") or []
        message = self.detail or self.title or f"The API returned HTTP {self.status}."
        super().__init__(f"{self.code or self.status}: {message}")


class APIConnectionError(NorbelysError):
    """A network error or timeout prevented the request from receiving a response."""

    def __init__(self, cause: httpx.TransportError) -> None:
        self.timeout = isinstance(cause, httpx.TimeoutException)
        super().__init__(
            "The request timed out." if self.timeout else "The API could not be reached."
        )
