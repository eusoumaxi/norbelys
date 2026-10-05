"""Wire behavior and failure recovery, using loop-free HTTPX transports without live credentials."""

import asyncio
import json
from datetime import UTC, datetime, timedelta
from email.utils import format_datetime
from pathlib import Path
from urllib.parse import quote

import httpx
import pytest

from norbelys import (
    APIConnectionError,
    APIError,
    AsyncNorbelys,
    Norbelys,
    NorbelysError,
    RequestOptions,
)
from norbelys._core import _delay
from norbelys.pagination import async_iter_items, iter_items

CONTRACT = json.loads((Path(__file__).parents[3] / "crates/server/openapi.json").read_text())
OPERATIONS = [
    (path, method, operation)
    for path, item in CONTRACT["paths"].items()
    for method, operation in item.items()
    if method in {"get", "post", "put", "patch", "delete"}
    and not operation["operationId"].startswith("health.")
]


@pytest.fixture(autouse=True)
def no_wait(monkeypatch):
    monkeypatch.delenv("NORBELYS_API_KEY", raising=False)
    monkeypatch.setattr("norbelys._core.time.sleep", lambda _: None)

    async def sleep(_):
        return None

    monkeypatch.setattr("norbelys._core.asyncio.sleep", sleep)


@pytest.mark.parametrize("asynchronous", [False, True])
@pytest.mark.parametrize(
    "path,method,operation", OPERATIONS, ids=[o[2]["operationId"] for o in OPERATIONS]
)
async def test_every_public_operation(path, method, operation, asynchronous):
    recorded = []

    def handler(request):
        recorded.append(request)
        return httpx.Response(200, json={"id": "result"})

    transport = httpx.MockTransport(handler)
    http = (
        httpx.AsyncClient(transport=transport)
        if asynchronous
        else httpx.Client(transport=transport)
    )
    client = (
        AsyncNorbelys(api_key="ak_test", http_client=http)
        if asynchronous
        else Norbelys(api_key="ak_test", http_client=http)
    )
    resource = client
    *names, action = operation["operationId"].split(".")
    for name in names:
        resource = getattr(resource, name)
    args = []
    expected = path
    for param in operation.get("parameters", []):
        if param["in"] == "path":
            value = "id /escaped?"
            args.append(value)
            expected = expected.replace("{" + param["name"] + "}", quote(value, safe=""))
    kwargs = {}
    media = operation.get("requestBody", {}).get("content", {})
    if media:
        if "application/json" in media:
            kwargs["body"] = {"name": "test"}
        elif "text/csv" in media:
            kwargs["body"] = "email\nuser@example.com"
        else:
            kwargs["body"] = {"data": b"image bytes", "content_type": "image/png"}
    result = getattr(resource, action)(*args, **kwargs)
    if asynchronous:
        await result
        await client.close()
        assert not http.is_closed
        await http.aclose()
    else:
        client.close()
        assert not http.is_closed
        http.close()
    assert len(recorded) == 1
    request = recorded[0]
    assert request.method == method.upper()
    assert request.url.raw_path.decode().split("?")[0] == expected
    assert request.headers["authorization"] == "Bearer ak_test"
    if media:
        assert request.content


@pytest.mark.parametrize("asynchronous", [False, True])
async def test_retries_preserve_write_and_refresh_tokens(asynchronous):
    requests = []
    tokens = iter(["one", "two"])

    def handler(request):
        requests.append(request)
        return (
            httpx.Response(503, headers={"retry-after": "0"})
            if len(requests) == 1
            else httpx.Response(200, json={"id": "created"})
        )

    http = (
        httpx.AsyncClient(transport=httpx.MockTransport(handler))
        if asynchronous
        else httpx.Client(transport=httpx.MockTransport(handler))
    )
    client = (
        AsyncNorbelys(token=lambda: next(tokens), http_client=http)
        if asynchronous
        else Norbelys(token=lambda: next(tokens), http_client=http)
    )
    result = client.groups.create({"name": "example"})
    if asynchronous:
        result = await result
        await http.aclose()
    else:
        http.close()
    assert result["id"] == "created"
    assert requests[0].headers["idempotency-key"] == requests[1].headers["idempotency-key"]
    assert requests[0].content == requests[1].content
    assert [r.headers["authorization"] for r in requests] == ["Bearer one", "Bearer two"]


@pytest.mark.parametrize("asynchronous", [False, True])
@pytest.mark.parametrize("failure", ["timeout", "network", "http", "unsafe"])
async def test_error_and_retry_boundaries(asynchronous, failure):
    requests = []

    def handler(request):
        requests.append(request)
        if failure == "timeout":
            raise httpx.ReadTimeout("private transport detail")
        if failure == "network":
            raise httpx.ConnectError("private transport detail")
        return httpx.Response(
            500,
            json={"code": "failed", "detail": "Failure"},
            headers={"retry-after": "0", "x-request-id": "req_1"},
        )

    http = (
        httpx.AsyncClient(transport=httpx.MockTransport(handler))
        if asynchronous
        else httpx.Client(transport=httpx.MockTransport(handler))
    )
    client = (
        AsyncNorbelys(api_key="ak_test", max_retries=1, http_client=http)
        if asynchronous
        else Norbelys(api_key="ak_test", max_retries=1, http_client=http)
    )
    error_type = APIConnectionError if failure in {"timeout", "network"} else APIError
    with pytest.raises(error_type) as raised:
        result = client._core.request("POST" if failure == "unsafe" else "GET", "/test", [])
        if asynchronous:
            await result
    assert len(requests) == (1 if failure == "unsafe" else 2)
    if isinstance(raised.value, APIConnectionError):
        assert raised.value.timeout == (failure == "timeout")
        assert "private transport detail" not in str(raised.value)
    else:
        assert raised.value.code == "failed"
        assert raised.value.request_id == "req_1"
    if asynchronous:
        await http.aclose()
    else:
        http.close()


@pytest.mark.parametrize(
    "options",
    [
        RequestOptions(if_match=17),
        RequestOptions(if_match='W/"17"', idempotency_key="same"),
        RequestOptions(if_match="*", headers={"x-extra": "value"}),
    ],
)
def test_request_encoding(options):
    recorded = []
    http = httpx.Client(
        transport=httpx.MockTransport(lambda r: recorded.append(r) or httpx.Response(204))
    )
    client = Norbelys(api_key="ak_test", http_client=http)
    assert (
        client._core.request(
            "DELETE",
            "/{id}",
            ["a/b"],
            query={"ids": ["x", None, "y"], "filter": {"x": 1}, "enabled": True, "omit": None},
            options=options,
        )
        is None
    )
    request = recorded[0]
    assert request.url.raw_path.startswith(b"/a%2Fb?")
    assert request.url.params.get_list("ids") == ["x", "y"]
    assert request.url.params["enabled"] == "true"
    assert request.url.params["filter"] == '{"x":1}'
    assert "omit" not in request.url.params
    assert request.headers["if-match"] == ('"17"' if options.if_match == 17 else options.if_match)
    if options.idempotency_key:
        assert request.headers["idempotency-key"] == "same"
    http.close()


@pytest.mark.parametrize(
    "config",
    [
        {},
        {"api_key": "a", "token": "b"},
        {"api_key": "a", "base_url": "ftp://example.com"},
        {"api_key": "a", "base_url": "https://u:p@example.com"},
        {"api_key": "a", "base_url": "https://example.com?key=x"},
        {"api_key": "a", "base_url": "https://example.com#x"},
        {"api_key": "a", "timeout": 0},
        {"api_key": "a", "timeout": float("nan")},
        {"api_key": "a", "max_retries": -1},
    ],
)
def test_invalid_client_configuration(config):
    with pytest.raises(NorbelysError):
        Norbelys(**config)


@pytest.mark.parametrize(
    "args,options",
    [
        ([], None),
        ([""], None),
        (["a", "b"], None),
        (["a"], RequestOptions(timeout=0)),
        (["a"], RequestOptions(max_retries=-1)),
    ],
)
def test_invalid_request_configuration(args, options):
    with Norbelys(api_key="a") as client, pytest.raises(NorbelysError):
        client._core.request("GET", "/{id}", args, options=options)


def test_token_failures_do_not_send():
    with Norbelys(token=lambda: "") as client, pytest.raises(NorbelysError):
        client.groups.list()


def test_environment_credentials_and_text_response(monkeypatch):
    monkeypatch.setenv("NORBELYS_API_KEY", "env_key")
    http = httpx.Client(
        transport=httpx.MockTransport(lambda r: httpx.Response(200, text="proxy text"))
    )
    with Norbelys(http_client=http) as client:
        assert client.groups.list() == "proxy text"
    http.close()


@pytest.mark.parametrize(
    "body",
    [
        {"title": "Bad", "request_id": "body_id", "errors": [{"path": "name"}]},
        [],
        "upstream failed",
    ],
)
def test_api_errors_retain_response(body):
    response = httpx.Response(400, headers={"x-request-id": "header_id"})
    error = APIError(response, body)
    assert error.response is response
    assert error.status == 400
    assert error.body == body
    assert error.request_id == (
        body.get("request_id", "header_id") if isinstance(body, dict) else "header_id"
    )


@pytest.mark.parametrize(
    "status,body,header,expected",
    [
        (400, {}, None, False),
        (409, {"code": "idempotency_in_progress"}, "0", True),
        (409, {}, None, False),
        (409, [], None, False),
        (503, {}, "61", False),
        (503, {}, "0", True),
        (503, {}, None, True),
        (503, {}, "soon", True),
        (503, {}, "nan", True),
        (503, {}, "-1", True),
        (503, {}, format_datetime(datetime.now(UTC) + timedelta(seconds=10)), True),
    ],
)
def test_retry_after(status, body, header, expected):
    response = httpx.Response(status, json=body, headers={"retry-after": header} if header else {})
    assert (_delay(response, 0) is not None) == expected


@pytest.mark.parametrize("asynchronous", [False, True])
async def test_lazy_pagination(asynchronous):
    requests = []

    def handler(request):
        requests.append(request)
        return httpx.Response(
            200,
            json={
                "data": [{"id": str(len(requests))}],
                "meta": {"has_more": len(requests) == 1, "next_cursor": "second"},
            },
        )

    http = (
        httpx.AsyncClient(transport=httpx.MockTransport(handler))
        if asynchronous
        else httpx.Client(transport=httpx.MockTransport(handler))
    )
    client = (
        AsyncNorbelys(api_key="a", http_client=http)
        if asynchronous
        else Norbelys(api_key="a", http_client=http)
    )
    items = client.groups.iter(query={"limit": 1})
    assert requests == []
    if asynchronous:
        result = [item async for item in items]
        await http.aclose()
    else:
        result = list(items)
        http.close()
    assert [r["id"] for r in result] == ["1", "2"]
    assert requests[1].url.params["cursor"] == "second"
    assert all(r.url.params["limit"] == "1" for r in requests)


@pytest.mark.parametrize("cursor", ["same", 123])
@pytest.mark.parametrize("asynchronous", [False, True])
async def test_pagination_rejects_loops(cursor, asynchronous):
    page = {"data": [], "meta": {"has_more": True, "next_cursor": cursor}}

    async def load(_):
        return page

    with pytest.raises(ValueError):
        if asynchronous:
            [item async for item in async_iter_items(load)]
        else:
            list(iter_items(lambda _: page))


async def test_owned_clients_and_async_context_close():
    async with AsyncNorbelys(api_key="a") as client:
        assert not client._core.client.is_closed
    assert client._core.client.is_closed
    with Norbelys(api_key="a") as sync:
        assert not sync._core.client.is_closed
    assert sync._core.client.is_closed


async def test_async_cancellation_propagates():
    async def handler(_):
        raise asyncio.CancelledError

    async with (
        httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http,
        AsyncNorbelys(api_key="a", http_client=http) as client,
    ):
        with pytest.raises(asyncio.CancelledError):
            await client.groups.list()
