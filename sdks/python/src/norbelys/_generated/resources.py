# Generated from the public OpenAPI contract. Do not edit.
"""Every public API operation with the same typed interface for sync and asyncio clients."""
from __future__ import annotations
from collections.abc import AsyncIterator, Iterator
from typing import Union, cast, overload
from .._core import AsyncCore, RequestOptions, SyncCore
from ..pagination import async_iter_items, iter_items
from . import models as schema

class Workspaces:
    """Typed operations for workspaces."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.WorkspaceObject:
        "Retrieve a workspace."
        return cast("schema.WorkspaceObject", self._core.request("GET", "/v1/workspaces/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class WebhookEndpoints:
    """Typed operations for webhook_endpoints."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.WebhookEndpointsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_EndpointObject:
        "List the workspace's webhook endpoints, newest first by default."
        return cast("schema.Page_EndpointObject", self._core.request("GET", "/v1/webhook_endpoints", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.WebhookEndpointsListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.WebhookEndpointsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.WebhookEndpointsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    def create(self, body: schema.CreateEndpoint, *, options: RequestOptions | None = None) -> schema.EndpointObject:
        "Create a webhook endpoint. Its signing secret (`whsec_…`) is in this response only."
        return cast("schema.EndpointObject", self._core.request("POST", "/v1/webhook_endpoints", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.EndpointObject:
        "Retrieve a webhook endpoint (without its secret)."
        return cast("schema.EndpointObject", self._core.request("GET", "/v1/webhook_endpoints/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def update(self, id: str, body: schema.UpdateEndpoint, *, options: RequestOptions | None = None) -> schema.EndpointObject:
        "Update a webhook endpoint: its URL, its event types, or whether it is enabled."
        return cast("schema.EndpointObject", self._core.request("PATCH", "/v1/webhook_endpoints/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete a webhook endpoint and its deliveries."
        return cast("None", self._core.request("DELETE", "/v1/webhook_endpoints/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def replay(self, id: str, body: schema.ReplayEndpoint, *, options: RequestOptions | None = None) -> schema.EndpointObject:
        "Replay an endpoint's deliveries."
        return cast("schema.EndpointObject", self._core.request("POST", "/v1/webhook_endpoints/{id}/replay", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    def rotate_secret(self, id: str, *, options: RequestOptions | None = None) -> schema.EndpointObject:
        "Rotate an endpoint's signing secret."
        return cast("schema.EndpointObject", self._core.request("POST", "/v1/webhook_endpoints/{id}/rotate_secret", [id], query=None, body=None, content_type=None, idempotent=True, options=options))

class WebhookDeliveries:
    """Typed operations for webhook_deliveries."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.WebhookDeliveriesListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_DeliveryObject:
        "List the workspace's webhook deliveries, newest events first by default."
        return cast("schema.Page_DeliveryObject", self._core.request("GET", "/v1/webhook_deliveries", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.WebhookDeliveriesListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.WebhookDeliveriesListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.WebhookDeliveriesListQuery, {**(query or {}), "cursor": cursor}), options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.DeliveryObject:
        "Retrieve a webhook delivery with its latest attempt."
        return cast("schema.DeliveryObject", self._core.request("GET", "/v1/webhook_deliveries/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def retry(self, id: str, *, options: RequestOptions | None = None) -> schema.DeliveryObject:
        "Retry a webhook delivery now."
        return cast("schema.DeliveryObject", self._core.request("POST", "/v1/webhook_deliveries/{id}/retry", [id], query=None, body=None, content_type=None, idempotent=True, options=options))

class Threads:
    """Typed operations for threads."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.ThreadsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_ThreadObject:
        "List the workspace's threads, newest first by default."
        return cast("schema.Page_ThreadObject", self._core.request("GET", "/v1/threads", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.ThreadsListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.ThreadsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.ThreadsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.ThreadObject:
        "Retrieve a thread with its latest 50 messages, outbound and inbound, oldest first."
        return cast("schema.ThreadObject", self._core.request("GET", "/v1/threads/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def update(self, id: str, body: schema.UpdateThread, *, options: RequestOptions | None = None) -> schema.ThreadObject:
        "Change a thread's status (open, snooze or archive it) or mark it read."
        return cast("schema.ThreadObject", self._core.request("PATCH", "/v1/threads/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

class Suppressions:
    """Typed operations for suppressions."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.SuppressionsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_SuppressionObject:
        "List the workspace's suppressions, newest first by default."
        return cast("schema.Page_SuppressionObject", self._core.request("GET", "/v1/suppressions", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.SuppressionsListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.SuppressionsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.SuppressionsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    @overload
    def create(self, body: schema.CreateSuppression, *, options: RequestOptions | None = None) -> schema.SuppressionObject: ...

    @overload
    def create(self, body: schema.CreateSuppressions, *, options: RequestOptions | None = None) -> schema.SuppressedAddresses: ...

    def create(self, body: schema.SuppressionForm, *, options: RequestOptions | None = None) -> Union[schema.SuppressedAddresses, schema.SuppressionObject]:
        "Suppress an address, or a list of them: no mail of the workspace reaches them any more."
        return cast("Union[schema.SuppressedAddresses, schema.SuppressionObject]", self._core.request("POST", "/v1/suppressions", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.SuppressionObject:
        "Retrieve a suppression."
        return cast("schema.SuppressionObject", self._core.request("GET", "/v1/suppressions/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Remove a manual suppression."
        return cast("None", self._core.request("DELETE", "/v1/suppressions/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class Smtp:
    """Typed operations for smtp."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def authenticate(self, *, query: schema.SmtpAuthenticateQuery | None = None, options: RequestOptions | None = None) -> schema.SmtpAuthorization:
        "Authenticate SMTP submission using the same live API key as HTTP sending."
        return cast("schema.SmtpAuthorization", self._core.request("GET", "/v1/smtp/auth", [], query=query, body=None, content_type=None, idempotent=False, options=options))

class SendingDomains:
    """Typed operations for sending_domains."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.SendingDomainsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_DomainObject:
        "List the workspace's sending domains, newest first by default."
        return cast("schema.Page_DomainObject", self._core.request("GET", "/v1/sending_domains", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.SendingDomainsListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.SendingDomainsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.SendingDomainsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    def create(self, body: schema.CreateDomain, *, options: RequestOptions | None = None) -> schema.DomainObject:
        "Create a sending domain, `pending_verification`, with the DNS records to publish."
        return cast("schema.DomainObject", self._core.request("POST", "/v1/sending_domains", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.DomainObject:
        "Retrieve a sending domain."
        return cast("schema.DomainObject", self._core.request("GET", "/v1/sending_domains/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def update(self, id: str, body: schema.UpdateDomain, *, options: RequestOptions | None = None) -> schema.DomainObject:
        "Update a domain's use and its optional separate tracking hostname."
        return cast("schema.DomainObject", self._core.request("PATCH", "/v1/sending_domains/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete a sending domain."
        return cast("None", self._core.request("DELETE", "/v1/sending_domains/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def verify(self, id: str, *, options: RequestOptions | None = None) -> schema.DomainObject:
        "Verify a sending domain now: it becomes `verifying` and its DNS records are checked."
        return cast("schema.DomainObject", self._core.request("POST", "/v1/sending_domains/{id}/verify", [id], query=None, body=None, content_type=None, idempotent=True, options=options))

class Segments:
    """Typed operations for segments."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.SegmentsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_SegmentObject:
        "List the workspace's segments, newest first by default. A list leaves the counts out."
        return cast("schema.Page_SegmentObject", self._core.request("GET", "/v1/segments", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.SegmentsListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.SegmentsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.SegmentsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    def create(self, body: schema.CreateSegment, *, options: RequestOptions | None = None) -> schema.SegmentObject:
        "Create a segment; the response counts its people."
        return cast("schema.SegmentObject", self._core.request("POST", "/v1/segments", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.SegmentObject:
        "Retrieve a segment with its people counted now (up to 10,000)."
        return cast("schema.SegmentObject", self._core.request("GET", "/v1/segments/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def update(self, id: str, body: schema.UpdateSegment, *, options: RequestOptions | None = None) -> schema.SegmentObject:
        "Rename a segment or replace its filter."
        return cast("schema.SegmentObject", self._core.request("PATCH", "/v1/segments/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete a segment; its people stay."
        return cast("None", self._core.request("DELETE", "/v1/segments/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class QuotaScopes:
    """Typed operations for quota_scopes."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.QuotaScopesListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_QuotaScopeObject:
        "List the workspace's quota scopes, newest first by default."
        return cast("schema.Page_QuotaScopeObject", self._core.request("GET", "/v1/quota_scopes", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.QuotaScopesListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.QuotaScopesListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.QuotaScopesListQuery, {**(query or {}), "cursor": cursor}), options=options))

    def create(self, body: schema.CreateQuotaScope, *, options: RequestOptions | None = None) -> schema.QuotaScopeObject:
        "Create a quota scope."
        return cast("schema.QuotaScopeObject", self._core.request("POST", "/v1/quota_scopes", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.QuotaScopeObject:
        "Retrieve a quota scope."
        return cast("schema.QuotaScopeObject", self._core.request("GET", "/v1/quota_scopes/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def update(self, id: str, body: schema.UpdateQuotaScope, *, options: RequestOptions | None = None) -> schema.QuotaScopeObject:
        "Update a quota scope's limits: each limit given replaces the stored one, `null` clears it."
        return cast("schema.QuotaScopeObject", self._core.request("PATCH", "/v1/quota_scopes/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete a quota scope and its ledger."
        return cast("None", self._core.request("DELETE", "/v1/quota_scopes/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class Preflight:
    """Typed operations for preflight."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def create(self, body: schema.CreatePreflight, *, options: RequestOptions | None = None) -> schema.PreflightResult:
        "Check addresses before mailing them."
        return cast("schema.PreflightResult", self._core.request("POST", "/v1/preflight", [], query=None, body=body, content_type=None, idempotent=False, options=options))

class People:
    """Typed operations for people."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.PeopleListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_PersonObject:
        "List the workspace's people, newest first by default."
        return cast("schema.Page_PersonObject", self._core.request("GET", "/v1/people", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.PeopleListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.PeopleListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.PeopleListQuery, {**(query or {}), "cursor": cursor}), options=options))

    def create(self, body: schema.CreatePerson, *, options: RequestOptions | None = None) -> schema.PersonObject:
        "Create a person."
        return cast("schema.PersonObject", self._core.request("POST", "/v1/people", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.PersonObject:
        "Retrieve a person."
        return cast("schema.PersonObject", self._core.request("GET", "/v1/people/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def update(self, id: str, body: schema.UpdatePerson, *, options: RequestOptions | None = None) -> schema.PersonObject:
        "Update a person: its address, names, company, custom values (merged) or groups (replaced)."
        return cast("schema.PersonObject", self._core.request("PATCH", "/v1/people/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete a person and its memberships."
        return cast("None", self._core.request("DELETE", "/v1/people/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class Metrics:
    """Typed operations for metrics."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def retrieve(self, *, query: schema.MetricsRetrieveQuery | None = None, options: RequestOptions | None = None) -> schema.Metrics:
        "Retrieve daily counts, rates or current workspace usage."
        return cast("schema.Metrics", self._core.request("GET", "/v1/metrics", [], query=query, body=None, content_type=None, idempotent=False, options=options))

class Messages:
    """Typed operations for messages."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.MessagesListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_MessageObject:
        "List the workspace's messages, newest first by default."
        return cast("schema.Page_MessageObject", self._core.request("GET", "/v1/messages", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.MessagesListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.MessagesListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.MessagesListQuery, {**(query or {}), "cursor": cursor}), options=options))

    @overload
    def create(self, body: schema.CreateMessage | schema.CreateReply | schema.CreateStepMessage, *, options: RequestOptions | None = None) -> schema.MessageObject: ...

    @overload
    def create(self, body: schema.CreateStepMessages, *, options: RequestOptions | None = None) -> schema.StepResults: ...

    def create(self, body: schema.MessageForm, *, options: RequestOptions | None = None) -> schema.MessagesCreated:
        "Send a message: it is queued now and sent when due, outside any campaign's cadence."
        return cast("schema.MessagesCreated", self._core.request("POST", "/v1/messages", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    def search(self, *, query: schema.MessagesSearchQuery | None = None, options: RequestOptions | None = None) -> schema.Page_HistoryMessage:
        "Search complete retained content, or list history when q is absent."
        return cast("schema.Page_HistoryMessage", self._core.request("GET", "/v1/messages/search", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.MessageObject:
        "Retrieve a message, with its latest attempts, its first delivery events and its holds."
        return cast("schema.MessageObject", self._core.request("GET", "/v1/messages/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def cancel(self, id: str, *, options: RequestOptions | None = None) -> schema.MessageObject:
        "Cancel a queued message."
        return cast("schema.MessageObject", self._core.request("POST", "/v1/messages/{id}/cancel", [id], query=None, body=None, content_type=None, idempotent=True, options=options))

    def content(self, id: str, *, options: RequestOptions | None = None) -> schema.MessageContent:
        "Retrieve an outbound message's retained authored or last prepared content."
        return cast("schema.MessageContent", self._core.request("GET", "/v1/messages/{id}/content", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def release_holds(self, id: str, body: schema.ReleaseHolds, *, options: RequestOptions | None = None) -> schema.MessageObject:
        "Release a message's holds."
        return cast("schema.MessageObject", self._core.request("POST", "/v1/messages/{id}/release_holds", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    def resolve(self, id: str, body: schema.ResolveMessage, *, options: RequestOptions | None = None) -> schema.MessageObject:
        "Resolve an uncertain message."
        return cast("schema.MessageObject", self._core.request("POST", "/v1/messages/{id}/resolve", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

class Jobs:
    """Typed operations for jobs."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.JobObject:
        "Retrieve a job of the credential's workspace."
        return cast("schema.JobObject", self._core.request("GET", "/v1/jobs/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def cancel(self, id: str, *, options: RequestOptions | None = None) -> schema.JobObject:
        "Request the cancellation of a job."
        return cast("schema.JobObject", self._core.request("POST", "/v1/jobs/{id}/cancel", [id], query=None, body=None, content_type=None, idempotent=True, options=options))

class InboundMessages:
    """Typed operations for inbound_messages."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.InboundMessagesListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_InboundMessageObject:
        "List the messages the inbox read, newest first by default."
        return cast("schema.Page_InboundMessageObject", self._core.request("GET", "/v1/inbound_messages", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.InboundMessagesListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.InboundMessagesListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.InboundMessagesListQuery, {**(query or {}), "cursor": cursor}), options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.InboundMessageObject:
        "Retrieve a message the inbox read."
        return cast("schema.InboundMessageObject", self._core.request("GET", "/v1/inbound_messages/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def update(self, id: str, body: schema.UpdateInbound, *, options: RequestOptions | None = None) -> schema.InboundMessageObject:
        "Correct a message's classification or sentiment by hand; AI never overrides it afterwards."
        return cast("schema.InboundMessageObject", self._core.request("PATCH", "/v1/inbound_messages/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    def content(self, id: str, *, options: RequestOptions | None = None) -> schema.MessageContent:
        "Retrieve full received content and available attachments, with truncation explicit."
        return cast("schema.MessageContent", self._core.request("GET", "/v1/inbound_messages/{id}/content", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def review(self, id: str, body: schema.Review, *, options: RequestOptions | None = None) -> schema.InboundMessageObject:
        "Confirm or dismiss what an inbound message proposed."
        return cast("schema.InboundMessageObject", self._core.request("POST", "/v1/inbound_messages/{id}/review", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

class Imports:
    """Typed operations for imports."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.ImportsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_ImportObject:
        "List the workspace's imports, newest first by default."
        return cast("schema.Page_ImportObject", self._core.request("GET", "/v1/imports", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.ImportsListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.ImportsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.ImportsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    def create(self, body: schema.CreateImport | str, *, query: schema.ImportsCreateQuery | None = None, options: RequestOptions | None = None) -> schema.ImportObject:
        "Import people from a CSV file or from JSON."
        return cast("schema.ImportObject", self._core.request("POST", "/v1/imports", [], query=query, body=body, content_type="text/csv" if isinstance(body, str) else None, idempotent=True, options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.ImportObject:
        "Retrieve an import: its status, counts, first problems and the link to its error report."
        return cast("schema.ImportObject", self._core.request("GET", "/v1/imports/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class Images:
    """Typed operations for images."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def create(self, body: schema.ImageUpload, *, options: RequestOptions | None = None) -> schema.ImageObject:
        "Upload an image to show in mail."
        return cast("schema.ImageObject", self._core.request("POST", "/v1/images", [], query=None, body=body["data"], content_type=body["content_type"], idempotent=True, options=options))

    def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete an image."
        return cast("None", self._core.request("DELETE", "/v1/images/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class Groups:
    """Typed operations for groups."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.GroupsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_GroupObject:
        "List the workspace's groups, newest first by default, each with its people counted."
        return cast("schema.Page_GroupObject", self._core.request("GET", "/v1/groups", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.GroupsListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.GroupsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.GroupsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    def create(self, body: schema.CreateGroup, *, options: RequestOptions | None = None) -> schema.GroupObject:
        "Create an empty group."
        return cast("schema.GroupObject", self._core.request("POST", "/v1/groups", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.GroupObject:
        "Retrieve a group with its people counted."
        return cast("schema.GroupObject", self._core.request("GET", "/v1/groups/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def update(self, id: str, body: schema.UpdateGroup, *, options: RequestOptions | None = None) -> schema.GroupObject:
        "Rename a group or change its description."
        return cast("schema.GroupObject", self._core.request("PATCH", "/v1/groups/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete a group and its memberships; its people stay."
        return cast("None", self._core.request("DELETE", "/v1/groups/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class Fields:
    """Typed operations for fields."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.FieldsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_FieldObject:
        "List the workspace's custom fields (at most 100)."
        return cast("schema.Page_FieldObject", self._core.request("GET", "/v1/fields", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.FieldsListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.FieldsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.FieldsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    def create(self, body: schema.CreateField, *, options: RequestOptions | None = None) -> schema.FieldObject:
        "Create a custom field."
        return cast("schema.FieldObject", self._core.request("POST", "/v1/fields", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    def update(self, id: str, body: schema.UpdateField, *, options: RequestOptions | None = None) -> schema.FieldObject:
        "Update a custom field's label or an enum's options. Its key and type never change."
        return cast("schema.FieldObject", self._core.request("PATCH", "/v1/fields/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete a custom field and every person's value of it."
        return cast("None", self._core.request("DELETE", "/v1/fields/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class Exports:
    """Typed operations for exports."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.ExportsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_ExportObject:
        "List the workspace's exports, newest first by default."
        return cast("schema.Page_ExportObject", self._core.request("GET", "/v1/exports", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.ExportsListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.ExportsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.ExportsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    def create(self, body: schema.CreateExport, *, options: RequestOptions | None = None) -> schema.ExportObject:
        "Export a resource's list to a file."
        return cast("schema.ExportObject", self._core.request("POST", "/v1/exports", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.ExportObject:
        "Retrieve an export, with a fresh download link when it is ready."
        return cast("schema.ExportObject", self._core.request("GET", "/v1/exports/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class Events:
    """Typed operations for events."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.EventsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_EventObject:
        "List the workspace's events, newest first by default."
        return cast("schema.Page_EventObject", self._core.request("GET", "/v1/events", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.EventsListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.EventsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.EventsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    def create(self, body: schema.CreateEvent, *, options: RequestOptions | None = None) -> schema.EventObject:
        "Create a synthetic event with sample data of its type."
        return cast("schema.EventObject", self._core.request("POST", "/v1/events", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.EventObject:
        "Retrieve an event."
        return cast("schema.EventObject", self._core.request("GET", "/v1/events/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class Enrollments:
    """Typed operations for enrollments."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.EnrollmentsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_EnrollmentObject:
        "List the workspace's enrollments, newest first by default."
        return cast("schema.Page_EnrollmentObject", self._core.request("GET", "/v1/enrollments", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.EnrollmentsListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.EnrollmentsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.EnrollmentsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    def create(self, body: schema.CreateEnrollments, *, options: RequestOptions | None = None) -> Union[schema.Enrolled, schema.JobObject]:
        "Enroll people into a campaign."
        return cast("Union[schema.Enrolled, schema.JobObject]", self._core.request("POST", "/v1/enrollments", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.EnrollmentObject:
        "Retrieve an enrollment."
        return cast("schema.EnrollmentObject", self._core.request("GET", "/v1/enrollments/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def stop(self, id: str, *, options: RequestOptions | None = None) -> schema.EnrollmentObject:
        "Stop an enrollment: no further step runs, and its message still queued is cancelled."
        return cast("schema.EnrollmentObject", self._core.request("POST", "/v1/enrollments/{id}/stop", [id], query=None, body=None, content_type=None, idempotent=True, options=options))

class DeliveryEvents:
    """Typed operations for delivery_events."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.DeliveryEventsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_DeliveryEventObject:
        "List the workspace's delivery events, newest first by default."
        return cast("schema.Page_DeliveryEventObject", self._core.request("GET", "/v1/delivery_events", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.DeliveryEventsListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.DeliveryEventsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.DeliveryEventsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.DeliveryEventObject:
        "Retrieve a delivery event."
        return cast("schema.DeliveryEventObject", self._core.request("GET", "/v1/delivery_events/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class Connections:
    """Typed operations for connections."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.ConnectionsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_ConnectionObject:
        "List the workspace's connections, newest first by default."
        return cast("schema.Page_ConnectionObject", self._core.request("GET", "/v1/connections", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.ConnectionsListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.ConnectionsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.ConnectionsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    def create(self, body: schema.CreateConnection, *, options: RequestOptions | None = None) -> Union[schema.ConnectionObject, schema.ConsentAnswer]:
        "Create a connection."
        return cast("Union[schema.ConnectionObject, schema.ConsentAnswer]", self._core.request("POST", "/v1/connections", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.ConnectionObject:
        "Retrieve a connection."
        return cast("schema.ConnectionObject", self._core.request("GET", "/v1/connections/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def update(self, id: str, body: schema.UpdateConnection, *, options: RequestOptions | None = None) -> schema.ConnectionObject:
        "Update a connection."
        return cast("schema.ConnectionObject", self._core.request("PATCH", "/v1/connections/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    def delete(self, id: str, *, options: RequestOptions | None = None) -> schema.ConnectionObject:
        "Archive a connection."
        return cast("schema.ConnectionObject", self._core.request("DELETE", "/v1/connections/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def verify(self, id: str, *, query: schema.ConnectionsVerifyQuery | None = None, options: RequestOptions | None = None) -> schema.ConnectionObject:
        "Verify a connection now."
        return cast("schema.ConnectionObject", self._core.request("POST", "/v1/connections/{id}/verify", [id], query=query, body=None, content_type=None, idempotent=True, options=options))

class Campaigns:
    """Typed operations for campaigns."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def list(self, *, query: schema.CampaignsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_CampaignObject:
        "List the workspace's campaigns, newest first by default; the variants' bodies are left out."
        return cast("schema.Page_CampaignObject", self._core.request("GET", "/v1/campaigns", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.CampaignsListQuery | None = None, options: RequestOptions | None = None) -> Iterator[schema.CampaignsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return iter_items(lambda cursor: self.list(query=cast(schema.CampaignsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    def create(self, body: schema.CreateCampaign, *, options: RequestOptions | None = None) -> schema.CampaignObject:
        "Create a campaign as a `draft`, with its steps, pool, schedule, tracking and stop rules."
        return cast("schema.CampaignObject", self._core.request("POST", "/v1/campaigns", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.CampaignObject:
        "Retrieve a campaign with its steps and their variants' bodies."
        return cast("schema.CampaignObject", self._core.request("GET", "/v1/campaigns/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def update(self, id: str, body: schema.UpdateCampaign, *, options: RequestOptions | None = None) -> schema.CampaignObject:
        "Change a campaign: its settings, its pool, or its steps."
        return cast("schema.CampaignObject", self._core.request("PATCH", "/v1/campaigns/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    def delete(self, id: str, *, options: RequestOptions | None = None) -> Union[schema.CampaignObject, None]:
        "Delete a campaign."
        return cast("Union[schema.CampaignObject, None]", self._core.request("DELETE", "/v1/campaigns/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def pause(self, id: str, *, options: RequestOptions | None = None) -> schema.CampaignObject:
        "Pause a campaign: no new message is created; queued messages wait until it is started again."
        return cast("schema.CampaignObject", self._core.request("POST", "/v1/campaigns/{id}/pause", [id], query=None, body=None, content_type=None, idempotent=True, options=options))

    def start(self, id: str, *, options: RequestOptions | None = None) -> schema.CampaignObject:
        "Start a campaign from `draft` or `paused`."
        return cast("schema.CampaignObject", self._core.request("POST", "/v1/campaigns/{id}/start", [id], query=None, body=None, content_type=None, idempotent=True, options=options))

class Attachments:
    """Typed operations for attachments."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def create(self, body: schema.Upload, *, options: RequestOptions | None = None) -> schema.AttachmentObject:
        "Upload an immutable attachment for use in messages or replies."
        return cast("schema.AttachmentObject", self._core.request("POST", "/v1/attachments", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.AttachmentObject:
        "Retrieve attachment metadata and a fresh download URL."
        return cast("schema.AttachmentObject", self._core.request("GET", "/v1/attachments/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete an unused attachment; a referenced file remains immutable."
        return cast("None", self._core.request("DELETE", "/v1/attachments/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class Analytics:
    """Typed operations for analytics."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core

    def retrieve(self, *, query: schema.AnalyticsRetrieveQuery | None = None, options: RequestOptions | None = None) -> schema.AnalyticsObject:
        "Retrieve the workspace's campaign counters."
        return cast("schema.AnalyticsObject", self._core.request("GET", "/v1/analytics", [], query=query, body=None, content_type=None, idempotent=False, options=options))

class NorbelysResources:
    """Typed operations for all public resources."""
    def __init__(self, core: SyncCore) -> None:
        self._core = core
        self.analytics = Analytics(core)
        self.attachments = Attachments(core)
        self.campaigns = Campaigns(core)
        self.connections = Connections(core)
        self.delivery_events = DeliveryEvents(core)
        self.enrollments = Enrollments(core)
        self.events = Events(core)
        self.exports = Exports(core)
        self.fields = Fields(core)
        self.groups = Groups(core)
        self.images = Images(core)
        self.imports = Imports(core)
        self.inbound_messages = InboundMessages(core)
        self.jobs = Jobs(core)
        self.messages = Messages(core)
        self.metrics = Metrics(core)
        self.people = People(core)
        self.preflight = Preflight(core)
        self.quota_scopes = QuotaScopes(core)
        self.segments = Segments(core)
        self.sending_domains = SendingDomains(core)
        self.smtp = Smtp(core)
        self.suppressions = Suppressions(core)
        self.threads = Threads(core)
        self.webhook_deliveries = WebhookDeliveries(core)
        self.webhook_endpoints = WebhookEndpoints(core)
        self.workspaces = Workspaces(core)

class AsyncWorkspaces:
    """Typed operations for workspaces."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.WorkspaceObject:
        "Retrieve a workspace."
        return cast("schema.WorkspaceObject", await self._core.request("GET", "/v1/workspaces/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class AsyncWebhookEndpoints:
    """Typed operations for webhook_endpoints."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.WebhookEndpointsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_EndpointObject:
        "List the workspace's webhook endpoints, newest first by default."
        return cast("schema.Page_EndpointObject", await self._core.request("GET", "/v1/webhook_endpoints", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.WebhookEndpointsListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.WebhookEndpointsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.WebhookEndpointsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    async def create(self, body: schema.CreateEndpoint, *, options: RequestOptions | None = None) -> schema.EndpointObject:
        "Create a webhook endpoint. Its signing secret (`whsec_…`) is in this response only."
        return cast("schema.EndpointObject", await self._core.request("POST", "/v1/webhook_endpoints", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.EndpointObject:
        "Retrieve a webhook endpoint (without its secret)."
        return cast("schema.EndpointObject", await self._core.request("GET", "/v1/webhook_endpoints/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def update(self, id: str, body: schema.UpdateEndpoint, *, options: RequestOptions | None = None) -> schema.EndpointObject:
        "Update a webhook endpoint: its URL, its event types, or whether it is enabled."
        return cast("schema.EndpointObject", await self._core.request("PATCH", "/v1/webhook_endpoints/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete a webhook endpoint and its deliveries."
        return cast("None", await self._core.request("DELETE", "/v1/webhook_endpoints/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def replay(self, id: str, body: schema.ReplayEndpoint, *, options: RequestOptions | None = None) -> schema.EndpointObject:
        "Replay an endpoint's deliveries."
        return cast("schema.EndpointObject", await self._core.request("POST", "/v1/webhook_endpoints/{id}/replay", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def rotate_secret(self, id: str, *, options: RequestOptions | None = None) -> schema.EndpointObject:
        "Rotate an endpoint's signing secret."
        return cast("schema.EndpointObject", await self._core.request("POST", "/v1/webhook_endpoints/{id}/rotate_secret", [id], query=None, body=None, content_type=None, idempotent=True, options=options))

class AsyncWebhookDeliveries:
    """Typed operations for webhook_deliveries."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.WebhookDeliveriesListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_DeliveryObject:
        "List the workspace's webhook deliveries, newest events first by default."
        return cast("schema.Page_DeliveryObject", await self._core.request("GET", "/v1/webhook_deliveries", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.WebhookDeliveriesListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.WebhookDeliveriesListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.WebhookDeliveriesListQuery, {**(query or {}), "cursor": cursor}), options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.DeliveryObject:
        "Retrieve a webhook delivery with its latest attempt."
        return cast("schema.DeliveryObject", await self._core.request("GET", "/v1/webhook_deliveries/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def retry(self, id: str, *, options: RequestOptions | None = None) -> schema.DeliveryObject:
        "Retry a webhook delivery now."
        return cast("schema.DeliveryObject", await self._core.request("POST", "/v1/webhook_deliveries/{id}/retry", [id], query=None, body=None, content_type=None, idempotent=True, options=options))

class AsyncThreads:
    """Typed operations for threads."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.ThreadsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_ThreadObject:
        "List the workspace's threads, newest first by default."
        return cast("schema.Page_ThreadObject", await self._core.request("GET", "/v1/threads", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.ThreadsListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.ThreadsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.ThreadsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.ThreadObject:
        "Retrieve a thread with its latest 50 messages, outbound and inbound, oldest first."
        return cast("schema.ThreadObject", await self._core.request("GET", "/v1/threads/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def update(self, id: str, body: schema.UpdateThread, *, options: RequestOptions | None = None) -> schema.ThreadObject:
        "Change a thread's status (open, snooze or archive it) or mark it read."
        return cast("schema.ThreadObject", await self._core.request("PATCH", "/v1/threads/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

class AsyncSuppressions:
    """Typed operations for suppressions."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.SuppressionsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_SuppressionObject:
        "List the workspace's suppressions, newest first by default."
        return cast("schema.Page_SuppressionObject", await self._core.request("GET", "/v1/suppressions", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.SuppressionsListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.SuppressionsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.SuppressionsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    @overload
    async def create(self, body: schema.CreateSuppression, *, options: RequestOptions | None = None) -> schema.SuppressionObject: ...

    @overload
    async def create(self, body: schema.CreateSuppressions, *, options: RequestOptions | None = None) -> schema.SuppressedAddresses: ...

    async def create(self, body: schema.SuppressionForm, *, options: RequestOptions | None = None) -> Union[schema.SuppressedAddresses, schema.SuppressionObject]:
        "Suppress an address, or a list of them: no mail of the workspace reaches them any more."
        return cast("Union[schema.SuppressedAddresses, schema.SuppressionObject]", await self._core.request("POST", "/v1/suppressions", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.SuppressionObject:
        "Retrieve a suppression."
        return cast("schema.SuppressionObject", await self._core.request("GET", "/v1/suppressions/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Remove a manual suppression."
        return cast("None", await self._core.request("DELETE", "/v1/suppressions/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class AsyncSmtp:
    """Typed operations for smtp."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def authenticate(self, *, query: schema.SmtpAuthenticateQuery | None = None, options: RequestOptions | None = None) -> schema.SmtpAuthorization:
        "Authenticate SMTP submission using the same live API key as HTTP sending."
        return cast("schema.SmtpAuthorization", await self._core.request("GET", "/v1/smtp/auth", [], query=query, body=None, content_type=None, idempotent=False, options=options))

class AsyncSendingDomains:
    """Typed operations for sending_domains."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.SendingDomainsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_DomainObject:
        "List the workspace's sending domains, newest first by default."
        return cast("schema.Page_DomainObject", await self._core.request("GET", "/v1/sending_domains", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.SendingDomainsListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.SendingDomainsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.SendingDomainsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    async def create(self, body: schema.CreateDomain, *, options: RequestOptions | None = None) -> schema.DomainObject:
        "Create a sending domain, `pending_verification`, with the DNS records to publish."
        return cast("schema.DomainObject", await self._core.request("POST", "/v1/sending_domains", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.DomainObject:
        "Retrieve a sending domain."
        return cast("schema.DomainObject", await self._core.request("GET", "/v1/sending_domains/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def update(self, id: str, body: schema.UpdateDomain, *, options: RequestOptions | None = None) -> schema.DomainObject:
        "Update a domain's use and its optional separate tracking hostname."
        return cast("schema.DomainObject", await self._core.request("PATCH", "/v1/sending_domains/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete a sending domain."
        return cast("None", await self._core.request("DELETE", "/v1/sending_domains/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def verify(self, id: str, *, options: RequestOptions | None = None) -> schema.DomainObject:
        "Verify a sending domain now: it becomes `verifying` and its DNS records are checked."
        return cast("schema.DomainObject", await self._core.request("POST", "/v1/sending_domains/{id}/verify", [id], query=None, body=None, content_type=None, idempotent=True, options=options))

class AsyncSegments:
    """Typed operations for segments."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.SegmentsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_SegmentObject:
        "List the workspace's segments, newest first by default. A list leaves the counts out."
        return cast("schema.Page_SegmentObject", await self._core.request("GET", "/v1/segments", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.SegmentsListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.SegmentsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.SegmentsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    async def create(self, body: schema.CreateSegment, *, options: RequestOptions | None = None) -> schema.SegmentObject:
        "Create a segment; the response counts its people."
        return cast("schema.SegmentObject", await self._core.request("POST", "/v1/segments", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.SegmentObject:
        "Retrieve a segment with its people counted now (up to 10,000)."
        return cast("schema.SegmentObject", await self._core.request("GET", "/v1/segments/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def update(self, id: str, body: schema.UpdateSegment, *, options: RequestOptions | None = None) -> schema.SegmentObject:
        "Rename a segment or replace its filter."
        return cast("schema.SegmentObject", await self._core.request("PATCH", "/v1/segments/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete a segment; its people stay."
        return cast("None", await self._core.request("DELETE", "/v1/segments/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class AsyncQuotaScopes:
    """Typed operations for quota_scopes."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.QuotaScopesListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_QuotaScopeObject:
        "List the workspace's quota scopes, newest first by default."
        return cast("schema.Page_QuotaScopeObject", await self._core.request("GET", "/v1/quota_scopes", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.QuotaScopesListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.QuotaScopesListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.QuotaScopesListQuery, {**(query or {}), "cursor": cursor}), options=options))

    async def create(self, body: schema.CreateQuotaScope, *, options: RequestOptions | None = None) -> schema.QuotaScopeObject:
        "Create a quota scope."
        return cast("schema.QuotaScopeObject", await self._core.request("POST", "/v1/quota_scopes", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.QuotaScopeObject:
        "Retrieve a quota scope."
        return cast("schema.QuotaScopeObject", await self._core.request("GET", "/v1/quota_scopes/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def update(self, id: str, body: schema.UpdateQuotaScope, *, options: RequestOptions | None = None) -> schema.QuotaScopeObject:
        "Update a quota scope's limits: each limit given replaces the stored one, `null` clears it."
        return cast("schema.QuotaScopeObject", await self._core.request("PATCH", "/v1/quota_scopes/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete a quota scope and its ledger."
        return cast("None", await self._core.request("DELETE", "/v1/quota_scopes/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class AsyncPreflight:
    """Typed operations for preflight."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def create(self, body: schema.CreatePreflight, *, options: RequestOptions | None = None) -> schema.PreflightResult:
        "Check addresses before mailing them."
        return cast("schema.PreflightResult", await self._core.request("POST", "/v1/preflight", [], query=None, body=body, content_type=None, idempotent=False, options=options))

class AsyncPeople:
    """Typed operations for people."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.PeopleListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_PersonObject:
        "List the workspace's people, newest first by default."
        return cast("schema.Page_PersonObject", await self._core.request("GET", "/v1/people", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.PeopleListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.PeopleListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.PeopleListQuery, {**(query or {}), "cursor": cursor}), options=options))

    async def create(self, body: schema.CreatePerson, *, options: RequestOptions | None = None) -> schema.PersonObject:
        "Create a person."
        return cast("schema.PersonObject", await self._core.request("POST", "/v1/people", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.PersonObject:
        "Retrieve a person."
        return cast("schema.PersonObject", await self._core.request("GET", "/v1/people/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def update(self, id: str, body: schema.UpdatePerson, *, options: RequestOptions | None = None) -> schema.PersonObject:
        "Update a person: its address, names, company, custom values (merged) or groups (replaced)."
        return cast("schema.PersonObject", await self._core.request("PATCH", "/v1/people/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete a person and its memberships."
        return cast("None", await self._core.request("DELETE", "/v1/people/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class AsyncMetrics:
    """Typed operations for metrics."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def retrieve(self, *, query: schema.MetricsRetrieveQuery | None = None, options: RequestOptions | None = None) -> schema.Metrics:
        "Retrieve daily counts, rates or current workspace usage."
        return cast("schema.Metrics", await self._core.request("GET", "/v1/metrics", [], query=query, body=None, content_type=None, idempotent=False, options=options))

class AsyncMessages:
    """Typed operations for messages."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.MessagesListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_MessageObject:
        "List the workspace's messages, newest first by default."
        return cast("schema.Page_MessageObject", await self._core.request("GET", "/v1/messages", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.MessagesListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.MessagesListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.MessagesListQuery, {**(query or {}), "cursor": cursor}), options=options))

    @overload
    async def create(self, body: schema.CreateMessage | schema.CreateReply | schema.CreateStepMessage, *, options: RequestOptions | None = None) -> schema.MessageObject: ...

    @overload
    async def create(self, body: schema.CreateStepMessages, *, options: RequestOptions | None = None) -> schema.StepResults: ...

    async def create(self, body: schema.MessageForm, *, options: RequestOptions | None = None) -> schema.MessagesCreated:
        "Send a message: it is queued now and sent when due, outside any campaign's cadence."
        return cast("schema.MessagesCreated", await self._core.request("POST", "/v1/messages", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def search(self, *, query: schema.MessagesSearchQuery | None = None, options: RequestOptions | None = None) -> schema.Page_HistoryMessage:
        "Search complete retained content, or list history when q is absent."
        return cast("schema.Page_HistoryMessage", await self._core.request("GET", "/v1/messages/search", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.MessageObject:
        "Retrieve a message, with its latest attempts, its first delivery events and its holds."
        return cast("schema.MessageObject", await self._core.request("GET", "/v1/messages/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def cancel(self, id: str, *, options: RequestOptions | None = None) -> schema.MessageObject:
        "Cancel a queued message."
        return cast("schema.MessageObject", await self._core.request("POST", "/v1/messages/{id}/cancel", [id], query=None, body=None, content_type=None, idempotent=True, options=options))

    async def content(self, id: str, *, options: RequestOptions | None = None) -> schema.MessageContent:
        "Retrieve an outbound message's retained authored or last prepared content."
        return cast("schema.MessageContent", await self._core.request("GET", "/v1/messages/{id}/content", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def release_holds(self, id: str, body: schema.ReleaseHolds, *, options: RequestOptions | None = None) -> schema.MessageObject:
        "Release a message's holds."
        return cast("schema.MessageObject", await self._core.request("POST", "/v1/messages/{id}/release_holds", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def resolve(self, id: str, body: schema.ResolveMessage, *, options: RequestOptions | None = None) -> schema.MessageObject:
        "Resolve an uncertain message."
        return cast("schema.MessageObject", await self._core.request("POST", "/v1/messages/{id}/resolve", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

class AsyncJobs:
    """Typed operations for jobs."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.JobObject:
        "Retrieve a job of the credential's workspace."
        return cast("schema.JobObject", await self._core.request("GET", "/v1/jobs/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def cancel(self, id: str, *, options: RequestOptions | None = None) -> schema.JobObject:
        "Request the cancellation of a job."
        return cast("schema.JobObject", await self._core.request("POST", "/v1/jobs/{id}/cancel", [id], query=None, body=None, content_type=None, idempotent=True, options=options))

class AsyncInboundMessages:
    """Typed operations for inbound_messages."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.InboundMessagesListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_InboundMessageObject:
        "List the messages the inbox read, newest first by default."
        return cast("schema.Page_InboundMessageObject", await self._core.request("GET", "/v1/inbound_messages", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.InboundMessagesListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.InboundMessagesListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.InboundMessagesListQuery, {**(query or {}), "cursor": cursor}), options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.InboundMessageObject:
        "Retrieve a message the inbox read."
        return cast("schema.InboundMessageObject", await self._core.request("GET", "/v1/inbound_messages/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def update(self, id: str, body: schema.UpdateInbound, *, options: RequestOptions | None = None) -> schema.InboundMessageObject:
        "Correct a message's classification or sentiment by hand; AI never overrides it afterwards."
        return cast("schema.InboundMessageObject", await self._core.request("PATCH", "/v1/inbound_messages/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def content(self, id: str, *, options: RequestOptions | None = None) -> schema.MessageContent:
        "Retrieve full received content and available attachments, with truncation explicit."
        return cast("schema.MessageContent", await self._core.request("GET", "/v1/inbound_messages/{id}/content", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def review(self, id: str, body: schema.Review, *, options: RequestOptions | None = None) -> schema.InboundMessageObject:
        "Confirm or dismiss what an inbound message proposed."
        return cast("schema.InboundMessageObject", await self._core.request("POST", "/v1/inbound_messages/{id}/review", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

class AsyncImports:
    """Typed operations for imports."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.ImportsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_ImportObject:
        "List the workspace's imports, newest first by default."
        return cast("schema.Page_ImportObject", await self._core.request("GET", "/v1/imports", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.ImportsListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.ImportsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.ImportsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    async def create(self, body: schema.CreateImport | str, *, query: schema.ImportsCreateQuery | None = None, options: RequestOptions | None = None) -> schema.ImportObject:
        "Import people from a CSV file or from JSON."
        return cast("schema.ImportObject", await self._core.request("POST", "/v1/imports", [], query=query, body=body, content_type="text/csv" if isinstance(body, str) else None, idempotent=True, options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.ImportObject:
        "Retrieve an import: its status, counts, first problems and the link to its error report."
        return cast("schema.ImportObject", await self._core.request("GET", "/v1/imports/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class AsyncImages:
    """Typed operations for images."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def create(self, body: schema.ImageUpload, *, options: RequestOptions | None = None) -> schema.ImageObject:
        "Upload an image to show in mail."
        return cast("schema.ImageObject", await self._core.request("POST", "/v1/images", [], query=None, body=body["data"], content_type=body["content_type"], idempotent=True, options=options))

    async def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete an image."
        return cast("None", await self._core.request("DELETE", "/v1/images/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class AsyncGroups:
    """Typed operations for groups."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.GroupsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_GroupObject:
        "List the workspace's groups, newest first by default, each with its people counted."
        return cast("schema.Page_GroupObject", await self._core.request("GET", "/v1/groups", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.GroupsListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.GroupsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.GroupsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    async def create(self, body: schema.CreateGroup, *, options: RequestOptions | None = None) -> schema.GroupObject:
        "Create an empty group."
        return cast("schema.GroupObject", await self._core.request("POST", "/v1/groups", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.GroupObject:
        "Retrieve a group with its people counted."
        return cast("schema.GroupObject", await self._core.request("GET", "/v1/groups/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def update(self, id: str, body: schema.UpdateGroup, *, options: RequestOptions | None = None) -> schema.GroupObject:
        "Rename a group or change its description."
        return cast("schema.GroupObject", await self._core.request("PATCH", "/v1/groups/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete a group and its memberships; its people stay."
        return cast("None", await self._core.request("DELETE", "/v1/groups/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class AsyncFields:
    """Typed operations for fields."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.FieldsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_FieldObject:
        "List the workspace's custom fields (at most 100)."
        return cast("schema.Page_FieldObject", await self._core.request("GET", "/v1/fields", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.FieldsListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.FieldsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.FieldsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    async def create(self, body: schema.CreateField, *, options: RequestOptions | None = None) -> schema.FieldObject:
        "Create a custom field."
        return cast("schema.FieldObject", await self._core.request("POST", "/v1/fields", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def update(self, id: str, body: schema.UpdateField, *, options: RequestOptions | None = None) -> schema.FieldObject:
        "Update a custom field's label or an enum's options. Its key and type never change."
        return cast("schema.FieldObject", await self._core.request("PATCH", "/v1/fields/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete a custom field and every person's value of it."
        return cast("None", await self._core.request("DELETE", "/v1/fields/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class AsyncExports:
    """Typed operations for exports."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.ExportsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_ExportObject:
        "List the workspace's exports, newest first by default."
        return cast("schema.Page_ExportObject", await self._core.request("GET", "/v1/exports", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.ExportsListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.ExportsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.ExportsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    async def create(self, body: schema.CreateExport, *, options: RequestOptions | None = None) -> schema.ExportObject:
        "Export a resource's list to a file."
        return cast("schema.ExportObject", await self._core.request("POST", "/v1/exports", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.ExportObject:
        "Retrieve an export, with a fresh download link when it is ready."
        return cast("schema.ExportObject", await self._core.request("GET", "/v1/exports/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class AsyncEvents:
    """Typed operations for events."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.EventsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_EventObject:
        "List the workspace's events, newest first by default."
        return cast("schema.Page_EventObject", await self._core.request("GET", "/v1/events", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.EventsListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.EventsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.EventsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    async def create(self, body: schema.CreateEvent, *, options: RequestOptions | None = None) -> schema.EventObject:
        "Create a synthetic event with sample data of its type."
        return cast("schema.EventObject", await self._core.request("POST", "/v1/events", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.EventObject:
        "Retrieve an event."
        return cast("schema.EventObject", await self._core.request("GET", "/v1/events/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class AsyncEnrollments:
    """Typed operations for enrollments."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.EnrollmentsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_EnrollmentObject:
        "List the workspace's enrollments, newest first by default."
        return cast("schema.Page_EnrollmentObject", await self._core.request("GET", "/v1/enrollments", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.EnrollmentsListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.EnrollmentsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.EnrollmentsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    async def create(self, body: schema.CreateEnrollments, *, options: RequestOptions | None = None) -> Union[schema.Enrolled, schema.JobObject]:
        "Enroll people into a campaign."
        return cast("Union[schema.Enrolled, schema.JobObject]", await self._core.request("POST", "/v1/enrollments", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.EnrollmentObject:
        "Retrieve an enrollment."
        return cast("schema.EnrollmentObject", await self._core.request("GET", "/v1/enrollments/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def stop(self, id: str, *, options: RequestOptions | None = None) -> schema.EnrollmentObject:
        "Stop an enrollment: no further step runs, and its message still queued is cancelled."
        return cast("schema.EnrollmentObject", await self._core.request("POST", "/v1/enrollments/{id}/stop", [id], query=None, body=None, content_type=None, idempotent=True, options=options))

class AsyncDeliveryEvents:
    """Typed operations for delivery_events."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.DeliveryEventsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_DeliveryEventObject:
        "List the workspace's delivery events, newest first by default."
        return cast("schema.Page_DeliveryEventObject", await self._core.request("GET", "/v1/delivery_events", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.DeliveryEventsListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.DeliveryEventsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.DeliveryEventsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.DeliveryEventObject:
        "Retrieve a delivery event."
        return cast("schema.DeliveryEventObject", await self._core.request("GET", "/v1/delivery_events/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class AsyncConnections:
    """Typed operations for connections."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.ConnectionsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_ConnectionObject:
        "List the workspace's connections, newest first by default."
        return cast("schema.Page_ConnectionObject", await self._core.request("GET", "/v1/connections", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.ConnectionsListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.ConnectionsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.ConnectionsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    async def create(self, body: schema.CreateConnection, *, options: RequestOptions | None = None) -> Union[schema.ConnectionObject, schema.ConsentAnswer]:
        "Create a connection."
        return cast("Union[schema.ConnectionObject, schema.ConsentAnswer]", await self._core.request("POST", "/v1/connections", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.ConnectionObject:
        "Retrieve a connection."
        return cast("schema.ConnectionObject", await self._core.request("GET", "/v1/connections/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def update(self, id: str, body: schema.UpdateConnection, *, options: RequestOptions | None = None) -> schema.ConnectionObject:
        "Update a connection."
        return cast("schema.ConnectionObject", await self._core.request("PATCH", "/v1/connections/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def delete(self, id: str, *, options: RequestOptions | None = None) -> schema.ConnectionObject:
        "Archive a connection."
        return cast("schema.ConnectionObject", await self._core.request("DELETE", "/v1/connections/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def verify(self, id: str, *, query: schema.ConnectionsVerifyQuery | None = None, options: RequestOptions | None = None) -> schema.ConnectionObject:
        "Verify a connection now."
        return cast("schema.ConnectionObject", await self._core.request("POST", "/v1/connections/{id}/verify", [id], query=query, body=None, content_type=None, idempotent=True, options=options))

class AsyncCampaigns:
    """Typed operations for campaigns."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def list(self, *, query: schema.CampaignsListQuery | None = None, options: RequestOptions | None = None) -> schema.Page_CampaignObject:
        "List the workspace's campaigns, newest first by default; the variants' bodies are left out."
        return cast("schema.Page_CampaignObject", await self._core.request("GET", "/v1/campaigns", [], query=query, body=None, content_type=None, idempotent=False, options=options))

    def iter(self, *, query: schema.CampaignsListQuery | None = None, options: RequestOptions | None = None) -> AsyncIterator[schema.CampaignsListItem]:
        """Fetch pages lazily and yield every item while preserving the original filters."""
        return async_iter_items(lambda cursor: self.list(query=cast(schema.CampaignsListQuery, {**(query or {}), "cursor": cursor}), options=options))

    async def create(self, body: schema.CreateCampaign, *, options: RequestOptions | None = None) -> schema.CampaignObject:
        "Create a campaign as a `draft`, with its steps, pool, schedule, tracking and stop rules."
        return cast("schema.CampaignObject", await self._core.request("POST", "/v1/campaigns", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.CampaignObject:
        "Retrieve a campaign with its steps and their variants' bodies."
        return cast("schema.CampaignObject", await self._core.request("GET", "/v1/campaigns/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def update(self, id: str, body: schema.UpdateCampaign, *, options: RequestOptions | None = None) -> schema.CampaignObject:
        "Change a campaign: its settings, its pool, or its steps."
        return cast("schema.CampaignObject", await self._core.request("PATCH", "/v1/campaigns/{id}", [id], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def delete(self, id: str, *, options: RequestOptions | None = None) -> Union[schema.CampaignObject, None]:
        "Delete a campaign."
        return cast("Union[schema.CampaignObject, None]", await self._core.request("DELETE", "/v1/campaigns/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def pause(self, id: str, *, options: RequestOptions | None = None) -> schema.CampaignObject:
        "Pause a campaign: no new message is created; queued messages wait until it is started again."
        return cast("schema.CampaignObject", await self._core.request("POST", "/v1/campaigns/{id}/pause", [id], query=None, body=None, content_type=None, idempotent=True, options=options))

    async def start(self, id: str, *, options: RequestOptions | None = None) -> schema.CampaignObject:
        "Start a campaign from `draft` or `paused`."
        return cast("schema.CampaignObject", await self._core.request("POST", "/v1/campaigns/{id}/start", [id], query=None, body=None, content_type=None, idempotent=True, options=options))

class AsyncAttachments:
    """Typed operations for attachments."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def create(self, body: schema.Upload, *, options: RequestOptions | None = None) -> schema.AttachmentObject:
        "Upload an immutable attachment for use in messages or replies."
        return cast("schema.AttachmentObject", await self._core.request("POST", "/v1/attachments", [], query=None, body=body, content_type=None, idempotent=True, options=options))

    async def retrieve(self, id: str, *, options: RequestOptions | None = None) -> schema.AttachmentObject:
        "Retrieve attachment metadata and a fresh download URL."
        return cast("schema.AttachmentObject", await self._core.request("GET", "/v1/attachments/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

    async def delete(self, id: str, *, options: RequestOptions | None = None) -> None:
        "Delete an unused attachment; a referenced file remains immutable."
        return cast("None", await self._core.request("DELETE", "/v1/attachments/{id}", [id], query=None, body=None, content_type=None, idempotent=False, options=options))

class AsyncAnalytics:
    """Typed operations for analytics."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core

    async def retrieve(self, *, query: schema.AnalyticsRetrieveQuery | None = None, options: RequestOptions | None = None) -> schema.AnalyticsObject:
        "Retrieve the workspace's campaign counters."
        return cast("schema.AnalyticsObject", await self._core.request("GET", "/v1/analytics", [], query=query, body=None, content_type=None, idempotent=False, options=options))

class AsyncNorbelysResources:
    """Typed operations for all public resources."""
    def __init__(self, core: AsyncCore) -> None:
        self._core = core
        self.analytics = AsyncAnalytics(core)
        self.attachments = AsyncAttachments(core)
        self.campaigns = AsyncCampaigns(core)
        self.connections = AsyncConnections(core)
        self.delivery_events = AsyncDeliveryEvents(core)
        self.enrollments = AsyncEnrollments(core)
        self.events = AsyncEvents(core)
        self.exports = AsyncExports(core)
        self.fields = AsyncFields(core)
        self.groups = AsyncGroups(core)
        self.images = AsyncImages(core)
        self.imports = AsyncImports(core)
        self.inbound_messages = AsyncInboundMessages(core)
        self.jobs = AsyncJobs(core)
        self.messages = AsyncMessages(core)
        self.metrics = AsyncMetrics(core)
        self.people = AsyncPeople(core)
        self.preflight = AsyncPreflight(core)
        self.quota_scopes = AsyncQuotaScopes(core)
        self.segments = AsyncSegments(core)
        self.sending_domains = AsyncSendingDomains(core)
        self.smtp = AsyncSmtp(core)
        self.suppressions = AsyncSuppressions(core)
        self.threads = AsyncThreads(core)
        self.webhook_deliveries = AsyncWebhookDeliveries(core)
        self.webhook_endpoints = AsyncWebhookEndpoints(core)
        self.workspaces = AsyncWorkspaces(core)
