# Generated from the public OpenAPI contract. Do not edit.
"""Static wire types. Values are ordinary JSON dictionaries; omitted and null remain distinct."""
from typing import Any, Literal, NotRequired, Required, TypedDict, TypeAlias, Union

Account = TypedDict("Account", {
    "email": Required[str],
    "issuer": NotRequired[Union[str, None]],
    "subject": NotRequired[Union[str, None]],
})

Address = TypedDict("Address", {
    "email": Required[str],
    "name": NotRequired[Union[str, None]],
})

AiSpend = TypedDict("AiSpend", {
    "budget_micros": Required[int],
    "reserved_micros": Required[int],
    "spent_micros": Required[int],
})

Allocation: TypeAlias = Union[Literal["balanced", "weighted", "automatic"], str]

AnalyticsGroup = TypedDict("AnalyticsGroup", {
    "campaign_id": NotRequired[Union[str, None]],
    "counters": Required["Counters"],
    "day": NotRequired[Union["Date", None]],
    "step_id": NotRequired[Union[str, None]],
    "variant_id": NotRequired[Union[str, None]],
    "variant_version": NotRequired[Union[int, None]],
})

AnalyticsObject = TypedDict("AnalyticsObject", {
    "computed_at": NotRequired[Union["Timestamp", None]],
    "data": Required[list["AnalyticsGroup"]],
    "from": Required["Date"],
    "group_by": NotRequired[Union["GroupBy", None]],
    "has_more": Required[bool],
    "policy": NotRequired[Union["PolicyReport", None]],
    "to": Required["Date"],
    "totals": Required["Counters"],
})

ApiCredentialInput = TypedDict("ApiCredentialInput", {
    "id": NotRequired[Union[str, None]],
    "secret": Required[str],
})

AttachmentObject = TypedDict("AttachmentObject", {
    "content_id": NotRequired[Union[str, None]],
    "content_type": Required[str],
    "created_at": Required["Timestamp"],
    "download_url": Required[str],
    "filename": Required[str],
    "id": Required["Id_Attachment"],
    "size_bytes": Required[int],
})

AttemptObject = TypedDict("AttemptObject", {
    "category": NotRequired[Union["EvidenceCategory", None]],
    "claimed_at": Required["Timestamp"],
    "connection_id": Required["Id_Connection"],
    "diagnostic": NotRequired[Union[str, None]],
    "enhanced_status": NotRequired[Union[str, None]],
    "finished_at": NotRequired[Union["Timestamp", None]],
    "id": Required["Id_Attempt"],
    "number": Required[int],
    "outcome": NotRequired[Union["AttemptOutcome", None]],
    "phase": NotRequired[Union["SubmissionPhase", None]],
    "provider_message_id": NotRequired[Union[str, None]],
    "recipient_count": Required[int],
    "smtp_code": NotRequired[Union[int, None]],
    "started_at": NotRequired[Union["Timestamp", None]],
})

AttemptOutcome: TypeAlias = Union[Literal["accepted", "transient", "permanent", "uncertain", "released", "suppressed", "skipped"], str]

Authorization = TypedDict("Authorization", {
    "expires_at": Required["Timestamp"],
    "url": Required[str],
})

CampaignLastError = TypedDict("CampaignLastError", {
    "at": Required["Timestamp"],
    "code": Required[str],
    "detail": Required[str],
})

CampaignObject = TypedDict("CampaignObject", {
    "created_at": Required["Timestamp"],
    "enrollments": NotRequired[Union["EnrollmentSummary", None]],
    "id": Required["Id_Campaign"],
    "last_error": NotRequired[Union["CampaignLastError", None]],
    "name": Required[str],
    "schedule": Required["ScheduleObject"],
    "senders": Required["SendersObject"],
    "stats": Required["StatsObject"],
    "status": Required["CampaignStatus"],
    "steps": Required[list["StepObject"]],
    "stop_rules": Required["StopRulesObject"],
    "tracking": Required["TrackingObject"],
    "updated_at": Required["Timestamp"],
    "version": Required[int],
})

CampaignStatus: TypeAlias = Union[Literal["draft", "materialising", "active", "paused", "completed", "archived"], str]

ClassificationSource: TypeAlias = Union[Literal["rules", "manual", "ai"], str]

Combine: TypeAlias = Union[Literal["all", "any"], str]

Condition = TypedDict("Condition", {
    "field": Required[str],
    "operator": Required["Operator"],
    "value": NotRequired[Any],
})

ConnectionObject = TypedDict("ConnectionObject", {
    "account": Required["Account"],
    "authorization": NotRequired[Union["Authorization", None]],
    "checked_at": NotRequired[Union["Timestamp", None]],
    "created_at": Required["Timestamp"],
    "created_by": NotRequired[Union[str, None]],
    "daily_limit": Required[int],
    "id": Required["Id_Connection"],
    "identities": Required[list["IdentityObject"]],
    "imap": NotRequired[Union["ImapSettings", None]],
    "paused": Required[bool],
    "paused_until": NotRequired[Union["Timestamp", None]],
    "provider": Required["Provider"],
    "quota_scope_id": NotRequired[Union[str, None]],
    "receiving": Required["ReceivingObject"],
    "send_interval_minutes": NotRequired[Union[int, None]],
    "send_window": NotRequired[Union["SendWindow", None]],
    "smtp": NotRequired[Union["SmtpSettings", None]],
    "status": Required["ConnectionStatus"],
    "status_detail": NotRequired[Union[str, None]],
    "timezone": Required[str],
    "transport": Required["Transport"],
    "updated_at": Required["Timestamp"],
    "usage": Required["Usage"],
    "version": Required[int],
    "warmup_stage": NotRequired[Union[int, None]],
    "webhook": NotRequired[Union["WebhookObject", None]],
})

ConnectionStatus: TypeAlias = Union[Literal["unverified", "verifying", "active", "authorization_required", "failed", "disabled", "archived"], str]

ConsentAnswer = TypedDict("ConsentAnswer", {
    "authorization": Required["Authorization"],
})

Counters = TypedDict("Counters", {
    "bounced": Required[int],
    "clicked": Required[int],
    "complained": Required[int],
    "delivered": Required[int],
    "opened": Required[int],
    "replied": Required[int],
    "sent": Required[int],
    "unsubscribed": Required[int],
})

CreateCampaign = TypedDict("CreateCampaign", {
    "name": Required[str],
    "schedule": NotRequired[Union["ScheduleInput", None]],
    "senders": NotRequired[Union["SendersInput", None]],
    "steps": NotRequired[Union[list["StepInput"], None]],
    "stop_rules": NotRequired[Union["StopRulesInput", None]],
    "tracking": NotRequired[Union["TrackingInput", None]],
})

CreateConnection = TypedDict("CreateConnection", {
    "account_email": NotRequired[Union[str, None]],
    "daily_limit": NotRequired[Union[int, None]],
    "identities": NotRequired[Union[list["IdentityInput"], None]],
    "imap": NotRequired[Union["ImapInput", None]],
    "provider": Required["Provider"],
    "quota_scope_id": NotRequired[Union[str, None]],
    "receiving": NotRequired[Union["ReceivingInput", None]],
    "return_to": NotRequired[Union[str, None]],
    "send_interval_minutes": NotRequired[Union[int, None]],
    "send_window": NotRequired[Union["SendWindow", None]],
    "smtp": NotRequired[Union["SmtpInput", None]],
    "timezone": NotRequired[Union[str, None]],
    "warmup_stage": NotRequired[Union[int, None]],
    "webhook": NotRequired[Union["WebhookInput", None]],
})

CreateDomain = TypedDict("CreateDomain", {
    "hostname": Required[str],
    "purpose": NotRequired[Union["DomainPurpose", None]],
    "tracking_enabled": NotRequired[Union[bool, None]],
    "tracking_hostname": NotRequired[Union[str, None]],
})

CreateEndpoint = TypedDict("CreateEndpoint", {
    "enabled": NotRequired[Union[bool, None]],
    "event_types": Required[list["EventType"]],
    "filters": NotRequired[Union["Filters", None]],
    "headers": NotRequired[Union[dict[str, str], None]],
    "url": Required[str],
})

CreateEnrollments = TypedDict("CreateEnrollments", {
    "campaign_id": Required[str],
    "emails": NotRequired[Union[list[str], None]],
    "group_id": NotRequired[Union[str, None]],
    "person_ids": NotRequired[Union[list[str], None]],
    "segment_id": NotRequired[Union[str, None]],
})

CreateEvent = TypedDict("CreateEvent", {
    "type": Required["EventType"],
    "webhook_endpoint_id": NotRequired[Union[str, None]],
})

CreateExport = TypedDict("CreateExport", {
    "filters": NotRequired[Union[dict[str, Any], None]],
    "format": NotRequired[Union["ExportFormat", None]],
    "resource": Required["ExportResource"],
})

CreateField = TypedDict("CreateField", {
    "key": Required[str],
    "label": Required[str],
    "options": NotRequired[Union[list[str], None]],
    "type": Required["FieldType"],
})

CreateGroup = TypedDict("CreateGroup", {
    "description": NotRequired[Union[str, None]],
    "name": Required[str],
})

CreateImport = TypedDict("CreateImport", {
    "group_id": NotRequired[Union[str, None]],
    "people": Required[list[dict[str, Any]]],
})

CreateMessage = TypedDict("CreateMessage", {
    "attachments": NotRequired[list["Id_Attachment"]],
    "bcc": NotRequired[Union[list[str], None]],
    "cc": NotRequired[Union[list[str], None]],
    "expires_at": NotRequired[Union["Timestamp", None]],
    "from": Required[str],
    "html": Required[str],
    "send_at": NotRequired[Union["Timestamp", None]],
    "subject": Required[str],
    "to": Required[list[str]],
    "variables": NotRequired[Union[dict[str, Any], None]],
})

CreatePerson = TypedDict("CreatePerson", {
    "company": NotRequired[Union[str, None]],
    "email": Required[str],
    "family_name": NotRequired[Union[str, None]],
    "fields": NotRequired[Union[dict[str, Any], None]],
    "given_name": NotRequired[Union[str, None]],
    "group_ids": NotRequired[Union[list[str], None]],
})

CreatePreflight = TypedDict("CreatePreflight", {
    "emails": Required[list[str]],
})

CreateQuotaScope = TypedDict("CreateQuotaScope", {
    "messages_per_day": NotRequired[Union[int, None]],
    "provider": Required["Provider"],
    "recipients_per_day": NotRequired[Union[int, None]],
    "scope_key": Required[str],
    "window_limit": NotRequired[Union[int, None]],
    "window_seconds": NotRequired[Union[int, None]],
    "window_unit": NotRequired[Union["WindowUnit", None]],
})

CreateReply = TypedDict("CreateReply", {
    "attachments": NotRequired[list["Id_Attachment"]],
    "bcc": NotRequired[Union[list[str], None]],
    "cc": NotRequired[Union[list[str], None]],
    "expires_at": NotRequired[Union["Timestamp", None]],
    "html": Required[str],
    "send_at": NotRequired[Union["Timestamp", None]],
    "subject": NotRequired[Union[str, None]],
    "thread_id": Required[str],
    "to": NotRequired[Union[list[str], None]],
    "variables": NotRequired[Union[dict[str, Any], None]],
})

CreateSegment = TypedDict("CreateSegment", {
    "filter": Required["Filter"],
    "name": Required[str],
})

CreateStepMessage = TypedDict("CreateStepMessage", {
    "from": NotRequired[Union[str, None]],
    "person_id": Required[str],
    "send_at": NotRequired[Union["Timestamp", None]],
    "step_id": NotRequired[Union[str, None]],
    "to": NotRequired[Union[str, None]],
    "variables": NotRequired[Union[dict[str, Any], None]],
    "variant_id": NotRequired[Union[str, None]],
})

CreateStepMessages = TypedDict("CreateStepMessages", {
    "from": NotRequired[Union[str, None]],
    "person_ids": Required[list[str]],
    "send_at": NotRequired[Union["Timestamp", None]],
    "step_id": NotRequired[Union[str, None]],
    "variables": NotRequired[Union[dict[str, Any], None]],
    "variant_id": NotRequired[Union[str, None]],
})

CreateSuppression = TypedDict("CreateSuppression", {
    "email": Required[str],
    "reason": NotRequired[Union["SuppressionReason", None]],
})

CreateSuppressions = TypedDict("CreateSuppressions", {
    "emails": Required[list[str]],
    "reason": NotRequired[Union["SuppressionReason", None]],
})

Date: TypeAlias = str

Decision: TypeAlias = Literal["confirm", "dismiss"]

DeliveryEventKind: TypeAlias = Union[Literal["accepted", "deferred", "delivered", "bounced", "rejected", "complaint", "unsubscribed", "address_changed", "reported"], str]

DeliveryEventObject = TypedDict("DeliveryEventObject", {
    "action": NotRequired[Union["DsnAction", None]],
    "attempt_number": NotRequired[Union[int, None]],
    "category": Required["EvidenceCategory"],
    "confidence": Required["EvidenceConfidence"],
    "diagnostic": NotRequired[Union[str, None]],
    "enhanced_status": NotRequired[Union[str, None]],
    "id": Required["Id_DeliveryEvent"],
    "kind": Required["DeliveryEventKind"],
    "message_id": NotRequired[Union["Id_Message", None]],
    "observed_at": Required["Timestamp"],
    "phase": NotRequired[Union["SubmissionPhase", None]],
    "processed_at": Required["Timestamp"],
    "provider_code": NotRequired[Union[str, None]],
    "received_at": Required["Timestamp"],
    "recipient": NotRequired[Union[str, None]],
    "recipient_ref": Required["RecipientRef"],
    "source": Required["EvidenceSource"],
    "thread_id": NotRequired[Union["Id_Thread", None]],
})

DeliveryObject = TypedDict("DeliveryObject", {
    "attempts": Required[int],
    "created_at": Required["Timestamp"],
    "delivered_at": NotRequired[Union["Timestamp", None]],
    "event_id": Required["Id_OutboxEvent"],
    "event_type": Required["EventType"],
    "id": Required["Id_WebhookDelivery"],
    "last_attempt": NotRequired[Union["LastAttempt", None]],
    "next_attempt_at": NotRequired[Union["Timestamp", None]],
    "state": Required["WebhookDeliveryState"],
    "webhook_endpoint_id": Required["Id_WebhookEndpoint"],
})

Direction: TypeAlias = Union[Literal["outbound", "inbound"], str]

DnsPreparation: TypeAlias = Union[Literal["preparing", "ready", "unavailable"], str]

DnsRecord = TypedDict("DnsRecord", {
    "name": Required[str],
    "note": NotRequired[Union[str, None]],
    "priority": NotRequired[Union[int, None]],
    "purpose": Required["DnsRecordPurpose"],
    "status": Required["DnsRecordStatus"],
    "type": Required["DnsRecordType"],
    "value": Required[str],
})

DnsRecordPurpose: TypeAlias = Union[Literal["ownership", "tracking", "spf", "dmarc", "dkim", "mx"], str]

DnsRecordStatus: TypeAlias = Union[Literal["verified", "missing", "unchecked"], str]

DnsRecordType: TypeAlias = Union[Literal["TXT", "CNAME", "MX"], str]

DomainObject = TypedDict("DomainObject", {
    "checked_at": NotRequired[Union["Timestamp", None]],
    "created_at": Required["Timestamp"],
    "dns_preparation": Required["DnsPreparation"],
    "existing_mx": Required[list["MailExchange"]],
    "hostname": Required[str],
    "id": Required["Id_SendingDomain"],
    "last_error": NotRequired[Union["LastError", None]],
    "purpose": Required["DomainPurpose"],
    "records": Required[list["DnsRecord"]],
    "status": Required["SendingDomainStatus"],
    "tracking_domain": NotRequired[Union["TrackingDomainObject", None]],
    "tracking_enabled": Required[bool],
    "updated_at": Required["Timestamp"],
    "verified_at": NotRequired[Union["Timestamp", None]],
    "version": Required[int],
    "warnings": Required[list[str]],
})

DomainPurpose: TypeAlias = Union[Literal["tracking", "send", "receive", "send_receive"], str]

DsnAction: TypeAlias = Union[Literal["failed", "delayed", "delivered", "relayed", "expanded"], str]

EndpointDisabledReason: TypeAlias = Union[Literal["gone", "failing", "manual"], str]

EndpointObject = TypedDict("EndpointObject", {
    "created_at": Required["Timestamp"],
    "disabled_reason": NotRequired[Union["EndpointDisabledReason", None]],
    "enabled": Required[bool],
    "event_types": Required[list["EventType"]],
    "failing_since": NotRequired[Union["Timestamp", None]],
    "filters": Required["Filters"],
    "header_names": Required[list[str]],
    "id": Required["Id_WebhookEndpoint"],
    "secret": NotRequired[Union[str, None]],
    "updated_at": Required["Timestamp"],
    "url": Required[str],
    "version": Required[int],
})

Enrolled = TypedDict("Enrolled", {
    "data": Required[list["EnrollmentObject"]],
    "skipped": Required[list["Skipped"]],
})

EnrollmentObject = TypedDict("EnrollmentObject", {
    "campaign_id": Required["Id_Campaign"],
    "created_at": Required["Timestamp"],
    "id": Required["Id_Enrollment"],
    "message_id": NotRequired[Union["Id_Message", None]],
    "next_run_at": NotRequired[Union["Timestamp", None]],
    "person": Required["PersonRef"],
    "position": Required[int],
    "sender_identity_id": NotRequired[Union["Id_SenderIdentity", None]],
    "status": Required["EnrollmentStatus"],
    "status_detail": NotRequired[Union[str, None]],
    "updated_at": Required["Timestamp"],
    "waiting_for": NotRequired[Union["Id_SenderIdentity", None]],
})

EnrollmentStatus: TypeAlias = Union[Literal["active", "paused", "completed", "replied", "stopped", "failed"], str]

EnrollmentSummary = TypedDict("EnrollmentSummary", {
    "active": Required[int],
    "completed": Required[int],
    "failed": Required[int],
    "next_run_at": NotRequired[Union["Timestamp", None]],
    "paused": Required[int],
    "replied": Required[int],
    "steps": Required[list["StepEnrollments"]],
    "stopped": Required[int],
})

EventObject = TypedDict("EventObject", {
    "created_at": Required["Timestamp"],
    "data": Required[Any],
    "id": Required["Id_OutboxEvent"],
    "synthetic": Required[bool],
    "type": Required["EventType"],
    "webhook_endpoint_id": NotRequired[Union["Id_WebhookEndpoint", None]],
})

EventType: TypeAlias = Union[Literal["message.queued", "message.sent", "message.failed", "message.uncertain", "message.cancelled", "message.snippets_fallback", "delivery_event.recorded", "inbound_message.received", "enrollment.stopped", "enrollment.completed", "campaign.status_changed", "connection.health_changed", "import.completed", "export.completed", "suppression.created", "ai.budget_warning", "ai.budget_exceeded", "endpoint.test", "webhook_endpoint.disabled"], str]

EvidenceCategory: TypeAlias = Union[Literal["accepted", "invalid_recipient", "mailbox_full", "no_route", "invalid_address", "content_rejected", "policy", "throttled", "unauthorized", "forbidden", "connection_failed", "deadline", "unsupported", "transient", "rejected", "uncertain", "delivered", "complaint", "unsubscribed", "address_changed", "expired", "suppressed", "sender_archived", "render_failed", "workspace_deleted"], str]

EvidenceConfidence: TypeAlias = Union[Literal["authenticated", "corroborated", "inferred", "human_text"], str]

EvidenceSource: TypeAlias = Union[Literal["smtp", "provider_api", "provider_webhook", "dsn", "arf", "inbound_notice", "preflight", "unsubscribe", "manual", "sent_folder"], str]

ExportFormat: TypeAlias = Union[Literal["csv", "jsonl"], str]

ExportObject = TypedDict("ExportObject", {
    "created_at": Required["Timestamp"],
    "expires_at": Required["Timestamp"],
    "filters": Required[dict[str, Any]],
    "format": Required["ExportFormat"],
    "id": Required["Id_Export"],
    "job_id": NotRequired[Union["Id_Job", None]],
    "last_error": NotRequired[Union["LastError", None]],
    "resource": Required["ExportResource"],
    "rows": NotRequired[Union[int, None]],
    "status": Required["ExportStatus"],
    "updated_at": Required["Timestamp"],
    "url": NotRequired[Union[str, None]],
})

ExportResource: TypeAlias = Union[Literal["people", "messages", "attempts", "delivery_events", "inbound_messages"], str]

ExportStatus: TypeAlias = Union[Literal["queued", "running", "ready", "failed", "expired"], str]

Family: TypeAlias = Union[Literal["events", "rates", "usage"], str]

FieldError = TypedDict("FieldError", {
    "code": Required[str],
    "detail": Required[str],
    "pointer": Required[str],
})

FieldObject = TypedDict("FieldObject", {
    "created_at": Required["Timestamp"],
    "id": Required["Id_Field"],
    "key": Required[str],
    "label": Required[str],
    "options": Required[list[str]],
    "type": Required["FieldType"],
    "updated_at": Required["Timestamp"],
    "version": Required[int],
})

FieldType: TypeAlias = Union[Literal["text", "number", "boolean", "enum", "date"], str]

Filter = TypedDict("Filter", {
    "conditions": Required[list["Condition"]],
    "match": NotRequired["Combine"],
})

Filters = TypedDict("Filters", {
    "campaign_ids": NotRequired[list["Id_Campaign"]],
    "connection_ids": NotRequired[list["Id_Connection"]],
})

Finding = TypedDict("Finding", {
    "detail": NotRequired[Union[str, None]],
    "email": Required[str],
    "hold": NotRequired[Union["HoldSummary", None]],
    "reason": Required["PreflightReason"],
    "smtp": NotRequired[Union["MailboxFinding", None]],
    "status": Required["PreflightStatus"],
    "suppression": NotRequired[Union["SuppressionSummary", None]],
})

FolderObject = TypedDict("FolderObject", {
    "enabled": Required[bool],
    "failures": Required[int],
    "folder": Required[str],
    "id": Required["Id_ReceiveBinding"],
    "polled_at": NotRequired[Union["Timestamp", None]],
    "status_detail": NotRequired[Union[str, None]],
})

GroupBy: TypeAlias = Union[Literal["day", "campaign", "step", "variant"], str]

GroupObject = TypedDict("GroupObject", {
    "created_at": Required["Timestamp"],
    "description": NotRequired[Union[str, None]],
    "id": Required["Id_Group"],
    "name": Required[str],
    "people_count": Required[int],
    "updated_at": Required["Timestamp"],
    "version": Required[int],
})

HistoryMessage = TypedDict("HistoryMessage", {
    "connection_id": Required["Id_Connection"],
    "created_at": Required["Timestamp"],
    "direction": Required["Direction"],
    "from_email": Required[str],
    "id": Required["MailId"],
    "subject": Required[str],
    "thread_id": NotRequired[Union["Id_Thread", None]],
    "truncated": Required[bool],
})

HoldObject = TypedDict("HoldObject", {
    "email": Required[str],
    "observed_at": Required["Timestamp"],
    "reason": Required["HoldReason"],
    "resolution": NotRequired[Union["HoldResolution", None]],
    "resolved_at": NotRequired[Union["Timestamp", None]],
    "review_after": Required["Timestamp"],
})

HoldReason: TypeAlias = Union[Literal["mailbox_full", "greylisted", "no_route", "invalid_recipient"], str]

HoldResolution: TypeAlias = Union[Literal["delivered", "expired", "suppressed", "manual"], str]

HoldSummary = TypedDict("HoldSummary", {
    "reason": Required["HoldReason"],
    "review_after": Required["Timestamp"],
})

Id_Attachment: TypeAlias = str

Id_Attempt: TypeAlias = str

Id_Campaign: TypeAlias = str

Id_Connection: TypeAlias = str

Id_DeliveryEvent: TypeAlias = str

Id_Enrollment: TypeAlias = str

Id_Export: TypeAlias = str

Id_Field: TypeAlias = str

Id_Group: TypeAlias = str

Id_Image: TypeAlias = str

Id_Import: TypeAlias = str

Id_InboundMessage: TypeAlias = str

Id_Job: TypeAlias = str

Id_Message: TypeAlias = str

Id_OutboxEvent: TypeAlias = str

Id_Person: TypeAlias = str

Id_ProviderWebhook: TypeAlias = str

Id_QuotaScope: TypeAlias = str

Id_ReceiveBinding: TypeAlias = str

Id_Segment: TypeAlias = str

Id_SenderIdentity: TypeAlias = str

Id_SendingDomain: TypeAlias = str

Id_Step: TypeAlias = str

Id_Suppression: TypeAlias = str

Id_Thread: TypeAlias = str

Id_Variant: TypeAlias = str

Id_WebhookDelivery: TypeAlias = str

Id_WebhookEndpoint: TypeAlias = str

Id_Workspace: TypeAlias = str

IdentityInput = TypedDict("IdentityInput", {
    "email": Required[str],
    "enabled": NotRequired[Union[bool, None]],
    "id": NotRequired[Union[str, None]],
    "name": NotRequired[Union[str, None]],
    "reply_to": NotRequired[Union[str, None]],
    "signature_html": NotRequired[Union[str, None]],
    "signature_text": NotRequired[Union[str, None]],
    "tags": NotRequired[Union[list[str], None]],
    "verified": NotRequired[Union[bool, None]],
})

IdentityObject = TypedDict("IdentityObject", {
    "created_at": Required["Timestamp"],
    "email": Required[str],
    "enabled": Required[bool],
    "id": Required["Id_SenderIdentity"],
    "name": NotRequired[Union[str, None]],
    "reply_to": NotRequired[Union[str, None]],
    "signature_html": NotRequired[Union[str, None]],
    "signature_text": NotRequired[Union[str, None]],
    "tags": Required[list[str]],
    "updated_at": Required["Timestamp"],
    "verified": Required[bool],
})

ImageContentType: TypeAlias = Union[Literal["image/png", "image/jpeg", "image/gif", "image/webp"], str]

ImageObject = TypedDict("ImageObject", {
    "content_type": Required["ImageContentType"],
    "created_at": Required["Timestamp"],
    "id": Required["Id_Image"],
    "size": Required[int],
    "url": Required[str],
})

ImapInput = TypedDict("ImapInput", {
    "host": Required[str],
    "port": Required[int],
    "security": Required["ImapSecurity"],
})

ImapSecurity: TypeAlias = Union[Literal["tls", "plain"], str]

ImapSettings = TypedDict("ImapSettings", {
    "host": Required[str],
    "port": Required[int],
    "security": Required["ImapSecurity"],
})

ImportCounts = TypedDict("ImportCounts", {
    "imported": Required[int],
    "invalid": Required[int],
    "skipped": Required[int],
    "total": Required[int],
})

ImportErrors = TypedDict("ImportErrors", {
    "data": Required[list["RowProblem"]],
    "has_more": Required[bool],
    "url": NotRequired[Union[str, None]],
})

ImportFormat: TypeAlias = Union[Literal["csv", "json"], str]

ImportObject = TypedDict("ImportObject", {
    "completed_at": NotRequired[Union["Timestamp", None]],
    "counts": Required["ImportCounts"],
    "created_at": Required["Timestamp"],
    "errors": Required["ImportErrors"],
    "format": Required["ImportFormat"],
    "group_id": NotRequired[Union[str, None]],
    "id": Required["Id_Import"],
    "job_id": NotRequired[Union["Id_Job", None]],
    "last_error": NotRequired[Union["LastError", None]],
    "status": Required["ImportStatus"],
    "updated_at": Required["Timestamp"],
})

ImportStatus: TypeAlias = Union[Literal["queued", "processing", "completed", "failed"], str]

InboundClassification: TypeAlias = Union[Literal["human_reply", "auto_reply", "out_of_office", "bounce", "complaint", "address_change", "unsubscribe", "unknown"], str]

InboundMessageObject = TypedDict("InboundMessageObject", {
    "classification": Required["InboundClassification"],
    "classification_source": Required["ClassificationSource"],
    "connection_id": Required["Id_Connection"],
    "created_at": Required["Timestamp"],
    "evidence": Required[str],
    "from": NotRequired[Union["Address", None]],
    "id": Required["Id_InboundMessage"],
    "in_reply_to": NotRequired[Union[str, None]],
    "internet_message_id": NotRequired[Union[str, None]],
    "message_id": NotRequired[Union["Id_Message", None]],
    "person_id": NotRequired[Union["Id_Person", None]],
    "received_at": Required["Timestamp"],
    "references": Required[list[str]],
    "review": Required["InboundReview"],
    "sentiment": NotRequired[Union["Sentiment", None]],
    "size_bytes": NotRequired[Union[int, None]],
    "subject": NotRequired[Union[str, None]],
    "text": NotRequired[Union[str, None]],
    "thread_id": NotRequired[Union["Id_Thread", None]],
    "truncated": Required[bool],
    "updated_at": Required["Timestamp"],
    "version": Required[int],
})

InboundReview = TypedDict("InboundReview", {
    "decision": NotRequired[Union["ReviewDecision", None]],
    "proposal": NotRequired[Union["ReviewProposal", None]],
    "requested_at": NotRequired[Union["Timestamp", None]],
    "reviewed_at": NotRequired[Union["Timestamp", None]],
})

InvalidAddress = TypedDict("InvalidAddress", {
    "detail": Required[str],
    "index": Required[int],
    "value": Required[str],
})

JobObject = TypedDict("JobObject", {
    "attempts": Required[int],
    "cancel_requested_at": NotRequired[Union["Timestamp", None]],
    "created_at": Required["Timestamp"],
    "finished_at": NotRequired[Union["Timestamp", None]],
    "id": Required["Id_Job"],
    "kind": Required[str],
    "last_error": NotRequired[Union["LastError", None]],
    "progress": NotRequired[Any],
    "result": NotRequired[Any],
    "state": Required["JobState"],
    "updated_at": Required["Timestamp"],
})

JobState: TypeAlias = Union[Literal["available", "running", "completed", "failed", "cancelled", "needs_review"], str]

LastAttempt = TypedDict("LastAttempt", {
    "duration_ms": NotRequired[Union[int, None]],
    "response_excerpt": NotRequired[Union[str, None]],
    "response_status": NotRequired[Union[int, None]],
    "started_at": Required["Timestamp"],
})

LastError = TypedDict("LastError", {
    "at": Required["Timestamp"],
    "code": Required[str],
    "detail": Required[str],
})

ListInclude: TypeAlias = Literal["total_count"]

ListOrder: TypeAlias = Literal["desc", "asc"]

MailExchange = TypedDict("MailExchange", {
    "hostname": Required[str],
    "priority": Required[int],
})

MailId: TypeAlias = Union["Id_Message", "Id_InboundMessage"]

MailboxFinding = TypedDict("MailboxFinding", {
    "detail": Required[str],
    "status": Required["MailboxStatus"],
})

MailboxStatus: TypeAlias = Union[Literal["accepted", "invalid", "unknown", "skipped"], str]

MessageAttempts = TypedDict("MessageAttempts", {
    "data": Required[list["AttemptObject"]],
    "has_more": Required[bool],
})

MessageContent = TypedDict("MessageContent", {
    "connection_id": Required["Id_Connection"],
    "created_at": Required["Timestamp"],
    "direction": Required["Direction"],
    "from_email": Required[str],
    "id": Required["MailId"],
    "subject": Required[str],
    "thread_id": NotRequired[Union["Id_Thread", None]],
    "truncated": Required[bool],
    "attachments": Required[list["AttachmentObject"]],
    "bcc": Required[list[str]],
    "cc": Required[list[str]],
    "headers": Required[list[list[str]]],
    "html": NotRequired[Union[str, None]],
    "prepared_at": NotRequired[Union["Timestamp", None]],
    "raw_download_url": NotRequired[Union[str, None]],
    "text": NotRequired[Union[str, None]],
    "to": Required[list[str]],
})

MessageEvents = TypedDict("MessageEvents", {
    "data": Required[list["DeliveryEventObject"]],
    "has_more": Required[bool],
    "url": Required[str],
})

MessageForm: TypeAlias = Union["CreateMessage", "CreateStepMessage", "CreateStepMessages", "CreateReply"]

MessageKind: TypeAlias = Union[Literal["campaign", "direct", "reply", "transactional"], str]

MessageObject = TypedDict("MessageObject", {
    "attempts": Required["MessageAttempts"],
    "attempts_count": Required[int],
    "bcc": Required[list[str]],
    "campaign_id": NotRequired[Union["Id_Campaign", None]],
    "cc": Required[list[str]],
    "connection_id": Required["Id_Connection"],
    "created_at": Required["Timestamp"],
    "enrollment_id": NotRequired[Union["Id_Enrollment", None]],
    "events": Required["MessageEvents"],
    "expires_at": NotRequired[Union["Timestamp", None]],
    "from": Required["Address"],
    "holds": Required[list["HoldObject"]],
    "id": Required["Id_Message"],
    "in_reply_to": NotRequired[Union[str, None]],
    "internet_message_id": Required[str],
    "kind": Required["MessageKind"],
    "person_id": NotRequired[Union["Id_Person", None]],
    "reply_to": NotRequired[Union[str, None]],
    "send_at": Required["Timestamp"],
    "sender_identity_id": Required["Id_SenderIdentity"],
    "sent_at": NotRequired[Union["Timestamp", None]],
    "snippets_fallback": NotRequired[Union["SnippetsFallback", None]],
    "state": Required["MessageState"],
    "status_detail": NotRequired[Union[str, None]],
    "step_id": NotRequired[Union["Id_Step", None]],
    "subject": Required[str],
    "thread_id": NotRequired[Union["Id_Thread", None]],
    "to": Required[list[str]],
    "tracking": Required["Tracking"],
    "updated_at": Required["Timestamp"],
    "variant_id": NotRequired[Union["Id_Variant", None]],
})

MessageState: TypeAlias = Union[Literal["queued", "claimed", "in_flight", "sent", "failed", "cancelled", "uncertain", "suppressed"], str]

MessagesCreated: TypeAlias = Union["MessageObject", "StepResults"]

Meta = TypedDict("Meta", {
    "has_more": Required[bool],
    "next_cursor": NotRequired[Union[str, None]],
    "total_count": NotRequired[Union[int, None]],
    "total_count_capped": NotRequired[Union[bool, None]],
})

Metered = TypedDict("Metered", {
    "limit": NotRequired[Union[int, None]],
    "used": Required[int],
})

MetricDay = TypedDict("MetricDay", {
    "counters": Required["Counters"],
    "day": Required["Date"],
})

Metrics: TypeAlias = dict[str, Any]

Objective: TypeAlias = Union[Literal["opens", "clicks", "replies"], str]

OnSenderRemoved: TypeAlias = Union[Literal["reassign", "stop"], str]

Operator: TypeAlias = Union[Literal["equals", "not_equals", "in", "starts_with", "exists", "not_exists", "gt", "gte", "lt", "lte"], str]

Page_CampaignObject = TypedDict("Page_CampaignObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_ConnectionObject = TypedDict("Page_ConnectionObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_DeliveryEventObject = TypedDict("Page_DeliveryEventObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_DeliveryObject = TypedDict("Page_DeliveryObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_DomainObject = TypedDict("Page_DomainObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_EndpointObject = TypedDict("Page_EndpointObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_EnrollmentObject = TypedDict("Page_EnrollmentObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_EventObject = TypedDict("Page_EventObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_ExportObject = TypedDict("Page_ExportObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_FieldObject = TypedDict("Page_FieldObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_GroupObject = TypedDict("Page_GroupObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_HistoryMessage = TypedDict("Page_HistoryMessage", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_ImportObject = TypedDict("Page_ImportObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_InboundMessageObject = TypedDict("Page_InboundMessageObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_MessageObject = TypedDict("Page_MessageObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_PersonObject = TypedDict("Page_PersonObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_QuotaScopeObject = TypedDict("Page_QuotaScopeObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_SegmentObject = TypedDict("Page_SegmentObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_SuppressionObject = TypedDict("Page_SuppressionObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

Page_ThreadObject = TypedDict("Page_ThreadObject", {
    "data": Required[list[dict[str, Any]]],
    "meta": Required["Meta"],
})

PersonObject = TypedDict("PersonObject", {
    "company": NotRequired[Union[str, None]],
    "created_at": Required["Timestamp"],
    "email": Required[str],
    "family_name": NotRequired[Union[str, None]],
    "fields": Required[dict[str, Any]],
    "given_name": NotRequired[Union[str, None]],
    "group_ids": Required[list["Id_Group"]],
    "id": Required["Id_Person"],
    "last_sent_at": NotRequired[Union["Timestamp", None]],
    "replied_at": NotRequired[Union["Timestamp", None]],
    "updated_at": Required["Timestamp"],
    "version": Required[int],
})

PersonRef = TypedDict("PersonRef", {
    "email": Required[str],
    "id": Required["Id_Person"],
    "name": NotRequired[Union[str, None]],
})

PolicyCounts = TypedDict("PolicyCounts", {
    "account_restricted": Required[int],
    "affected": Required[int],
    "failed": Required[int],
    "pending": Required[int],
    "recovered": Required[int],
})

PolicyGroup = TypedDict("PolicyGroup", {
    "campaign_id": NotRequired[Union[str, None]],
    "counts": Required["PolicyCounts"],
    "day": NotRequired[Union["Date", None]],
    "step_id": NotRequired[Union[str, None]],
    "variant_id": NotRequired[Union[str, None]],
    "variant_version": NotRequired[Union[int, None]],
})

PolicyReport = TypedDict("PolicyReport", {
    "computed_at": Required["Timestamp"],
    "data": Required[list["PolicyGroup"]],
    "has_more": Required[bool],
    "totals": Required["PolicyCounts"],
})

PreflightReason: TypeAlias = Union[Literal["mx", "implicit_mx", "syntax", "no_domain", "null_mx", "no_route", "dns_unavailable"], str]

PreflightResult = TypedDict("PreflightResult", {
    "data": Required[list["Finding"]],
})

PreflightStatus: TypeAlias = Union[Literal["routable", "invalid", "unknown"], str]

Problem = TypedDict("Problem", {
    "code": Required["ProblemCode"],
    "detail": Required[str],
    "errors": NotRequired[list["FieldError"]],
    "instance": Required[str],
    "request_id": Required[str],
    "retry_after": NotRequired[Union[int, None]],
    "status": Required[int],
    "title": Required[str],
    "type": Required[str],
})

ProblemCode: TypeAlias = Union[Literal["invalid_request", "unauthorized", "forbidden", "session_required", "not_found", "archived", "method_not_allowed", "conflict", "invalid_state", "idempotency_in_progress", "precondition_failed", "payload_too_large", "unsupported_media_type", "validation_failed", "idempotency_mismatch", "suppressed", "captcha_failed", "insufficient_scope", "rate_limited", "quota_exceeded", "internal_error", "service_unavailable", "timeout"], str]

Provider: TypeAlias = Union[Literal["smtp", "google", "microsoft", "ses", "sendgrid", "mailgun", "norbelys"], str]

QuotaScopeObject = TypedDict("QuotaScopeObject", {
    "created_at": Required["Timestamp"],
    "id": Required["Id_QuotaScope"],
    "messages_per_day": NotRequired[Union[int, None]],
    "paused_detail": NotRequired[Union[str, None]],
    "paused_until": NotRequired[Union["Timestamp", None]],
    "provider": Required["Provider"],
    "recipients_per_day": NotRequired[Union[int, None]],
    "scope_key": Required[str],
    "updated_at": Required["Timestamp"],
    "version": Required[int],
    "webhook": NotRequired[Union["WebhookObject", None]],
    "window_limit": NotRequired[Union[int, None]],
    "window_seconds": NotRequired[Union[int, None]],
    "window_unit": NotRequired[Union["WindowUnit", None]],
})

Rates = TypedDict("Rates", {
    "bounced": NotRequired[Union[int, None]],
    "clicked": NotRequired[Union[int, None]],
    "complained": NotRequired[Union[int, None]],
    "delivered": NotRequired[Union[int, None]],
    "opened": NotRequired[Union[int, None]],
    "replied": NotRequired[Union[int, None]],
    "unsubscribed": NotRequired[Union[int, None]],
})

ReceivingInput = TypedDict("ReceivingInput", {
    "folders": Required[list[str]],
})

ReceivingObject = TypedDict("ReceivingObject", {
    "folders": Required[list["FolderObject"]],
})

RecipientRef: TypeAlias = Union[Literal["named", "single_envelope", "unknown"], str]

ReleaseHolds = TypedDict("ReleaseHolds", {
    "evidence": Required[str],
})

ReplayEndpoint = TypedDict("ReplayEndpoint", {
    "since": Required["Timestamp"],
})

Resolution: TypeAlias = Literal["sent", "failed"]

ResolveMessage = TypedDict("ResolveMessage", {
    "evidence": Required[str],
    "state": Required["Resolution"],
})

Review = TypedDict("Review", {
    "decision": Required["Decision"],
})

ReviewDecision: TypeAlias = Union[Literal["confirmed", "dismissed"], str]

ReviewProposal: TypeAlias = dict[str, Any]

RowProblem = TypedDict("RowProblem", {
    "field": Required[str],
    "problem": Required[str],
    "row": Required[int],
})

ScheduleInput = TypedDict("ScheduleInput", {
    "send_window": NotRequired[Union["SendWindow", None]],
    "start_at": NotRequired[Union[str, None]],
    "timezone": NotRequired[Union[str, None]],
})

ScheduleObject = TypedDict("ScheduleObject", {
    "send_window": NotRequired[Union["SendWindow", None]],
    "start_at": NotRequired[Union["Timestamp", None]],
    "timezone": Required[str],
})

SegmentObject = TypedDict("SegmentObject", {
    "computed_at": NotRequired[Union["Timestamp", None]],
    "created_at": Required["Timestamp"],
    "filter": Required["Filter"],
    "id": Required["Id_Segment"],
    "name": Required[str],
    "people_count": NotRequired[Union[int, None]],
    "people_count_capped": NotRequired[Union[bool, None]],
    "updated_at": Required["Timestamp"],
    "version": Required[int],
})

SendWindow = TypedDict("SendWindow", {
    "days": Required[list[int]],
    "end": Required[str],
    "start": Required[str],
})

SendersInput = TypedDict("SendersInput", {
    "identity_ids": NotRequired[Union[list[str], None]],
    "on_sender_removed": NotRequired[Union["OnSenderRemoved", None]],
    "tags": NotRequired[Union[list[str], None]],
})

SendersObject = TypedDict("SendersObject", {
    "identity_ids": Required[list["Id_SenderIdentity"]],
    "on_sender_removed": Required["OnSenderRemoved"],
    "tags": Required[list[str]],
})

SendingDomainStatus: TypeAlias = Union[Literal["pending_verification", "verifying", "verified", "pending_certificate", "active", "suspended", "deleting"], str]

Sentiment: TypeAlias = Union[Literal["positive", "neutral", "negative"], str]

SkipReason: TypeAlias = Union[Literal["not_found", "suppressed", "already_enrolled"], str]

Skipped = TypedDict("Skipped", {
    "email": NotRequired[Union[str, None]],
    "person_id": NotRequired[Union["Id_Person", None]],
    "reason": Required["SkipReason"],
})

SmtpAuthorization = TypedDict("SmtpAuthorization", {
    "username": Required[str],
})

SmtpInput = TypedDict("SmtpInput", {
    "configuration_set": NotRequired[Union[str, None]],
    "host": Required[str],
    "password": Required[str],
    "port": Required[int],
    "security": Required["SmtpSecurity"],
    "username": NotRequired[Union[str, None]],
})

SmtpPatch = TypedDict("SmtpPatch", {
    "configuration_set": NotRequired[Union[str, None]],
    "host": NotRequired[Union[str, None]],
    "password": NotRequired[Union[str, None]],
    "port": NotRequired[Union[int, None]],
    "security": NotRequired[Union["SmtpSecurity", None]],
    "username": NotRequired[Union[str, None]],
})

SmtpSecurity: TypeAlias = Union[Literal["tls", "starttls", "plain"], str]

SmtpSettings = TypedDict("SmtpSettings", {
    "configuration_set": NotRequired[Union[str, None]],
    "host": Required[str],
    "port": Required[int],
    "security": Required["SmtpSecurity"],
    "username": Required[str],
})

SnippetsFallback: TypeAlias = Union[Literal["off", "unavailable", "unusable", "over_budget", "paused", "deadline", "refused", "truncated", "invalid", "provider"], str]

StatsObject = TypedDict("StatsObject", {
    "bounced": Required[int],
    "clicked": Required[int],
    "computed_at": NotRequired[Union["Timestamp", None]],
    "opened": Required[int],
    "replied": Required[int],
    "sent": Required[int],
    "unsubscribed": Required[int],
})

StepEnrollments = TypedDict("StepEnrollments", {
    "live": Required[int],
    "step_id": Required["Id_Step"],
})

StepFailure = TypedDict("StepFailure", {
    "code": Required["ProblemCode"],
    "detail": Required[str],
    "errors": Required[list["FieldError"]],
})

StepInput = TypedDict("StepInput", {
    "allocation": NotRequired[Union["Allocation", None]],
    "delay_seconds": NotRequired[Union[int, None]],
    "id": NotRequired[Union["Id_Step", None]],
    "name": NotRequired[Union[str, None]],
    "personalisation_prompt": NotRequired[Union[str, None]],
    "same_thread": NotRequired[Union[bool, None]],
    "variants": NotRequired[Union[list["VariantInput"], None]],
    "winner_rule": NotRequired[Union["WinnerRuleInput", None]],
    "winner_variant_id": NotRequired[Union[str, None]],
})

StepObject = TypedDict("StepObject", {
    "allocation": Required["Allocation"],
    "delay_seconds": Required[int],
    "id": Required["Id_Step"],
    "name": Required[str],
    "personalisation_prompt": NotRequired[Union[str, None]],
    "position": Required[int],
    "revision": Required[int],
    "same_thread": Required[bool],
    "variants": Required[list["VariantObject"]],
    "winner": NotRequired[Union["WinnerObject", None]],
    "winner_rule": Required["WinnerRuleObject"],
})

StepResult = TypedDict("StepResult", {
    "error": NotRequired[Union["StepFailure", None]],
    "message": NotRequired[Union["MessageObject", None]],
    "person_id": Required[str],
})

StepResults = TypedDict("StepResults", {
    "data": Required[list["StepResult"]],
})

StopOnReply: TypeAlias = Union[Literal["all", "campaign", "none"], str]

StopRulesInput = TypedDict("StopRulesInput", {
    "company_on_reply": NotRequired[Union[bool, None]],
    "cooldown_hours": NotRequired[Union[int, None]],
    "on_reply": NotRequired[Union["StopOnReply", None]],
})

StopRulesObject = TypedDict("StopRulesObject", {
    "company_on_reply": Required[bool],
    "cooldown_hours": Required[int],
    "on_reply": Required["StopOnReply"],
})

SubmissionPhase: TypeAlias = Union[Literal["connect", "auth", "mail_from", "rcpt_to", "data", "api"], str]

SuppressedAddresses = TypedDict("SuppressedAddresses", {
    "already": Required[int],
    "created": Required[int],
    "invalid": Required[list["InvalidAddress"]],
})

SuppressionForm: TypeAlias = Union["CreateSuppression", "CreateSuppressions"]

SuppressionObject = TypedDict("SuppressionObject", {
    "created_at": Required["Timestamp"],
    "email": Required[str],
    "evidence": NotRequired[Union[dict[str, Any], None]],
    "id": Required["Id_Suppression"],
    "reason": Required["SuppressionReason"],
    "source": Required["SuppressionSource"],
})

SuppressionReason: TypeAlias = Union[Literal["unsubscribe", "bounce", "complaint", "manual", "address_changed", "account_closed", "no_mail_service"], str]

SuppressionSource: TypeAlias = Union[Literal["manual", "unsubscribe", "smtp", "provider_api", "provider_webhook", "dsn", "arf", "inbound_notice"], str]

SuppressionSummary = TypedDict("SuppressionSummary", {
    "id": Required["Id_Suppression"],
    "reason": Required["SuppressionReason"],
})

ThreadMessage = TypedDict("ThreadMessage", {
    "at": Required["Timestamp"],
    "classification": NotRequired[Union["InboundClassification", None]],
    "direction": Required["Direction"],
    "from": NotRequired[Union[str, None]],
    "id": Required[str],
    "state": NotRequired[Union["MessageState", None]],
    "subject": NotRequired[Union[str, None]],
    "text": NotRequired[Union[str, None]],
    "to": Required[list[str]],
})

ThreadMessages = TypedDict("ThreadMessages", {
    "data": Required[list["ThreadMessage"]],
    "has_more": Required[bool],
})

ThreadObject = TypedDict("ThreadObject", {
    "campaign_id": NotRequired[Union["Id_Campaign", None]],
    "connection_id": Required["Id_Connection"],
    "created_at": Required["Timestamp"],
    "id": Required["Id_Thread"],
    "last_activity_at": Required["Timestamp"],
    "last_message": NotRequired[Union["ThreadMessage", None]],
    "messages": NotRequired[Union["ThreadMessages", None]],
    "participants": Required[list[str]],
    "person_id": NotRequired[Union["Id_Person", None]],
    "sender_identity_id": Required["Id_SenderIdentity"],
    "snoozed_until": NotRequired[Union["Timestamp", None]],
    "status": Required["ThreadStatus"],
    "subject": NotRequired[Union[str, None]],
    "unread": Required[bool],
    "updated_at": Required["Timestamp"],
    "version": Required[int],
})

ThreadStatus: TypeAlias = Union[Literal["open", "snoozed", "archived"], str]

Timestamp: TypeAlias = str

Tracking = TypedDict("Tracking", {
    "clicks": Required[bool],
    "hostname": NotRequired[Union[str, None]],
    "opens": Required[bool],
})

TrackingDomainObject = TypedDict("TrackingDomainObject", {
    "checked_at": NotRequired[Union["Timestamp", None]],
    "hostname": Required[str],
    "id": Required["Id_SendingDomain"],
    "records": Required[list["DnsRecord"]],
    "status": Required[str],
    "verified_at": NotRequired[Union["Timestamp", None]],
})

TrackingInput = TypedDict("TrackingInput", {
    "clicks": NotRequired[Union[bool, None]],
    "domain_id": NotRequired[Union[str, None]],
    "opens": NotRequired[Union[bool, None]],
})

TrackingObject = TypedDict("TrackingObject", {
    "clicks": Required[bool],
    "domain_id": NotRequired[Union["Id_SendingDomain", None]],
    "opens": Required[bool],
})

Transport: TypeAlias = Union[Literal["smtp", "api"], str]

UpdateCampaign = TypedDict("UpdateCampaign", {
    "name": NotRequired[Union[str, None]],
    "schedule": NotRequired[Union["ScheduleInput", None]],
    "senders": NotRequired[Union["SendersInput", None]],
    "steps": NotRequired[Union[list["StepInput"], None]],
    "stop_rules": NotRequired[Union["StopRulesInput", None]],
    "tracking": NotRequired[Union["TrackingInput", None]],
})

UpdateConnection = TypedDict("UpdateConnection", {
    "api_credential": NotRequired[Union["ApiCredentialInput", None]],
    "daily_limit": NotRequired[Union[int, None]],
    "identities": NotRequired[Union[list["IdentityInput"], None]],
    "imap": NotRequired[Union["ImapInput", None]],
    "paused": NotRequired[Union[bool, None]],
    "quota_scope_id": NotRequired[Union[str, None]],
    "receiving": NotRequired[Union["ReceivingInput", None]],
    "send_interval_minutes": NotRequired[Union[int, None]],
    "send_window": NotRequired[Union["SendWindow", None]],
    "smtp": NotRequired[Union["SmtpPatch", None]],
    "timezone": NotRequired[Union[str, None]],
    "warmup_stage": NotRequired[Union[int, None]],
    "webhook": NotRequired[Union["WebhookInput", None]],
})

UpdateDomain = TypedDict("UpdateDomain", {
    "purpose": NotRequired[Union["DomainPurpose", None]],
    "tracking_enabled": NotRequired[Union[bool, None]],
    "tracking_hostname": NotRequired[Union[str, None]],
})

UpdateEndpoint = TypedDict("UpdateEndpoint", {
    "enabled": NotRequired[Union[bool, None]],
    "event_types": NotRequired[Union[list["EventType"], None]],
    "filters": NotRequired[Union["Filters", None]],
    "headers": NotRequired[Union[dict[str, str], None]],
    "url": NotRequired[Union[str, None]],
})

UpdateField = TypedDict("UpdateField", {
    "label": NotRequired[Union[str, None]],
    "options": NotRequired[Union[list[str], None]],
})

UpdateGroup = TypedDict("UpdateGroup", {
    "description": NotRequired[Union[str, None]],
    "name": NotRequired[Union[str, None]],
})

UpdateInbound = TypedDict("UpdateInbound", {
    "classification": NotRequired[Union["InboundClassification", None]],
    "sentiment": NotRequired[Union["Sentiment", None]],
})

UpdatePerson = TypedDict("UpdatePerson", {
    "company": NotRequired[Union[str, None]],
    "email": NotRequired[Union[str, None]],
    "family_name": NotRequired[Union[str, None]],
    "fields": NotRequired[Union[dict[str, Any], None]],
    "given_name": NotRequired[Union[str, None]],
    "group_ids": NotRequired[Union[list[str], None]],
})

UpdateQuotaScope = TypedDict("UpdateQuotaScope", {
    "messages_per_day": NotRequired[Union[int, None]],
    "recipients_per_day": NotRequired[Union[int, None]],
    "window_limit": NotRequired[Union[int, None]],
    "window_seconds": NotRequired[Union[int, None]],
    "window_unit": NotRequired[Union["WindowUnit", None]],
})

UpdateSegment = TypedDict("UpdateSegment", {
    "filter": NotRequired[Union["Filter", None]],
    "name": NotRequired[Union[str, None]],
})

UpdateThread = TypedDict("UpdateThread", {
    "snoozed_until": NotRequired[Union["Timestamp", None]],
    "status": NotRequired[Union["ThreadStatus", None]],
    "unread": NotRequired[Union[bool, None]],
})

Upload = TypedDict("Upload", {
    "content_base64": Required[str],
    "content_type": Required[str],
    "filename": Required[str],
})

Usage = TypedDict("Usage", {
    "today": Required["UsageDay"],
    "yesterday": Required["UsageDay"],
})

UsageDay = TypedDict("UsageDay", {
    "reserved": Required[int],
    "used": Required[int],
})

VariantInput = TypedDict("VariantInput", {
    "bcc": NotRequired[Union[list[str], None]],
    "cc": NotRequired[Union[list[str], None]],
    "html": NotRequired[Union[str, None]],
    "id": NotRequired[Union["Id_Variant", None]],
    "name": NotRequired[Union[str, None]],
    "preheader": NotRequired[Union[str, None]],
    "subject": NotRequired[Union[str, None]],
    "weight": NotRequired[Union[int, None]],
})

VariantObject = TypedDict("VariantObject", {
    "bcc": Required[list[str]],
    "cc": Required[list[str]],
    "html": NotRequired[str],
    "id": Required["Id_Variant"],
    "name": Required[str],
    "preheader": NotRequired[Union[str, None]],
    "subject": Required[str],
    "version": Required[int],
    "weight": Required[int],
})

WebhookDeliveryState: TypeAlias = Union[Literal["pending", "delivered", "failed", "disabled"], str]

WebhookInput = TypedDict("WebhookInput", {
    "key": Required[str],
})

WebhookObject = TypedDict("WebhookObject", {
    "id": Required["Id_ProviderWebhook"],
    "key_set": Required[bool],
    "url": Required[str],
})

WindowUnit: TypeAlias = Union[Literal["requests", "recipients", "units"], str]

WinnerObject = TypedDict("WinnerObject", {
    "selected_at": Required["Timestamp"],
    "selected_by": Required[str],
    "variant_id": Required["Id_Variant"],
})

WinnerRuleInput = TypedDict("WinnerRuleInput", {
    "minimum_sample": NotRequired[Union[int, None]],
    "objective": NotRequired[Union["Objective", None]],
    "observation_window_seconds": NotRequired[Union[int, None]],
})

WinnerRuleObject = TypedDict("WinnerRuleObject", {
    "minimum_sample": Required[int],
    "objective": Required["Objective"],
    "observation_window_seconds": Required[int],
})

WorkspaceMode: TypeAlias = Union[Literal["live", "test"], str]

WorkspaceObject = TypedDict("WorkspaceObject", {
    "created_at": Required["Timestamp"],
    "deletion_requested_at": NotRequired[Union["Timestamp", None]],
    "id": Required["Id_Workspace"],
    "mode": Required["WorkspaceMode"],
    "name": Required[str],
    "settings": Required[Any],
    "slug": Required[str],
    "timezone": Required[str],
    "updated_at": Required["Timestamp"],
    "usage": NotRequired[Union["WorkspaceUsage", None]],
    "version": Required[int],
})

WorkspaceUsage = TypedDict("WorkspaceUsage", {
    "ai": Required["AiSpend"],
    "computed_at": Required["Timestamp"],
    "connections": Required["Metered"],
    "month": Required["Date"],
    "people": Required["Metered"],
    "sends": Required["Metered"],
})

AnalyticsRetrieveQuery = TypedDict("AnalyticsRetrieveQuery", {
    "campaign_id": NotRequired["Id_Campaign"],
    "step_id": NotRequired["Id_Step"],
    "from": NotRequired[str],
    "to": NotRequired[str],
    "group_by": NotRequired["GroupBy"],
    "include_policy": NotRequired[Union[bool, None]],
})

CampaignsListQuery = TypedDict("CampaignsListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
    "status": NotRequired["CampaignStatus"],
    "q": NotRequired[str],
})

ConnectionsListQuery = TypedDict("ConnectionsListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
    "status": NotRequired["ConnectionStatus"],
    "provider": NotRequired["Provider"],
    "tag": NotRequired[str],
    "quota_scope_id": NotRequired["Id_QuotaScope"],
    "q": NotRequired[str],
})

ConnectionsVerifyQuery = TypedDict("ConnectionsVerifyQuery", {
    "return_to": NotRequired[str],
})

DeliveryEventsListQuery = TypedDict("DeliveryEventsListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
    "message_id": NotRequired["Id_Message"],
    "campaign_id": NotRequired["Id_Campaign"],
    "person_id": NotRequired["Id_Person"],
    "kind": NotRequired["DeliveryEventKind"],
    "recipient": NotRequired[str],
})

EnrollmentsListQuery = TypedDict("EnrollmentsListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
    "campaign_id": NotRequired["Id_Campaign"],
    "person_id": NotRequired["Id_Person"],
    "sender_identity_id": NotRequired["Id_SenderIdentity"],
    "status": NotRequired["EnrollmentStatus"],
})

EventsListQuery = TypedDict("EventsListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
    "type": NotRequired["EventType"],
    "after": NotRequired["Id_OutboxEvent"],
    "wait": NotRequired[int],
})

ExportsListQuery = TypedDict("ExportsListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
    "status": NotRequired[Literal["queued", "running", "ready", "failed"]],
})

FieldsListQuery = TypedDict("FieldsListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
})

GroupsListQuery = TypedDict("GroupsListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
    "q": NotRequired[str],
})

ImportsListQuery = TypedDict("ImportsListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
    "status": NotRequired["ImportStatus"],
})

ImportsCreateQuery = TypedDict("ImportsCreateQuery", {
    "group_id": NotRequired["Id_Group"],
})

InboundMessagesListQuery = TypedDict("InboundMessagesListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
    "thread_id": NotRequired["Id_Thread"],
    "connection_id": NotRequired["Id_Connection"],
    "classification": NotRequired["InboundClassification"],
    "review_requested": NotRequired[bool],
    "received_at[gte]": NotRequired[str],
})

MessagesListQuery = TypedDict("MessagesListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
    "campaign_id": NotRequired["Id_Campaign"],
    "enrollment_id": NotRequired["Id_Enrollment"],
    "person_id": NotRequired["Id_Person"],
    "connection_id": NotRequired["Id_Connection"],
    "thread_id": NotRequired["Id_Thread"],
    "state": NotRequired["MessageState"],
    "created_at[gte]": NotRequired[str],
    "created_at[gt]": NotRequired[str],
    "created_at[lte]": NotRequired[str],
    "created_at[lt]": NotRequired[str],
})

MessagesSearchQuery = TypedDict("MessagesSearchQuery", {
    "q": NotRequired[str],
    "thread_id": NotRequired["Id_Thread"],
    "connection_id": NotRequired["Id_Connection"],
    "direction": NotRequired["Direction"],
    "from": NotRequired["Timestamp"],
    "to": NotRequired["Timestamp"],
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
})

MetricsRetrieveQuery = TypedDict("MetricsRetrieveQuery", {
    "family": NotRequired["Family"],
    "from": NotRequired["Date"],
    "to": NotRequired["Date"],
    "connection_id": NotRequired["Id_Connection"],
    "campaign_id": NotRequired["Id_Campaign"],
    "kind": NotRequired["MessageKind"],
})

PeopleListQuery = TypedDict("PeopleListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
    "sort": NotRequired[str],
    "email": NotRequired[str],
    "group_id": NotRequired["Id_Group"],
    "segment_id": NotRequired["Id_Segment"],
    "q": NotRequired[str],
    "created_at[gte]": NotRequired[str],
    "created_at[gt]": NotRequired[str],
    "created_at[lte]": NotRequired[str],
    "created_at[lt]": NotRequired[str],
})

QuotaScopesListQuery = TypedDict("QuotaScopesListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
    "provider": NotRequired["Provider"],
})

SegmentsListQuery = TypedDict("SegmentsListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
})

SendingDomainsListQuery = TypedDict("SendingDomainsListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
    "status": NotRequired["SendingDomainStatus"],
})

SmtpAuthorizationRetrieveQuery = TypedDict("SmtpAuthorizationRetrieveQuery", {
    "domain": Required[str],
})

SuppressionsListQuery = TypedDict("SuppressionsListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
    "email": NotRequired[str],
    "reason": NotRequired["SuppressionReason"],
    "source": NotRequired["SuppressionSource"],
})

ThreadsListQuery = TypedDict("ThreadsListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "sort": NotRequired[str],
    "include": NotRequired["ListInclude"],
    "person_id": NotRequired["Id_Person"],
    "connection_id": NotRequired["Id_Connection"],
    "status": NotRequired["ThreadStatus"],
    "classification": NotRequired["InboundClassification"],
})

WebhookDeliveriesListQuery = TypedDict("WebhookDeliveriesListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
    "webhook_endpoint_id": NotRequired["Id_WebhookEndpoint"],
    "event_id": NotRequired["Id_OutboxEvent"],
    "state": NotRequired["WebhookDeliveryState"],
})

WebhookEndpointsListQuery = TypedDict("WebhookEndpointsListQuery", {
    "limit": NotRequired[int],
    "cursor": NotRequired[str],
    "order": NotRequired["ListOrder"],
    "include": NotRequired["ListInclude"],
    "enabled": NotRequired[bool],
})

ImageUpload = TypedDict("ImageUpload", {"data": bytes, "content_type": Literal["image/gif", "image/jpeg", "image/png", "image/webp"]})

WebhookEndpointsListItem: TypeAlias = dict[str, Any]

WebhookDeliveriesListItem: TypeAlias = dict[str, Any]

ThreadsListItem: TypeAlias = dict[str, Any]

SuppressionsListItem: TypeAlias = dict[str, Any]

SendingDomainsListItem: TypeAlias = dict[str, Any]

SegmentsListItem: TypeAlias = dict[str, Any]

QuotaScopesListItem: TypeAlias = dict[str, Any]

PeopleListItem: TypeAlias = dict[str, Any]

MessagesListItem: TypeAlias = dict[str, Any]

InboundMessagesListItem: TypeAlias = dict[str, Any]

ImportsListItem: TypeAlias = dict[str, Any]

GroupsListItem: TypeAlias = dict[str, Any]

FieldsListItem: TypeAlias = dict[str, Any]

ExportsListItem: TypeAlias = dict[str, Any]

EventsListItem: TypeAlias = dict[str, Any]

EnrollmentsListItem: TypeAlias = dict[str, Any]

DeliveryEventsListItem: TypeAlias = dict[str, Any]

ConnectionsListItem: TypeAlias = dict[str, Any]

CampaignsListItem: TypeAlias = dict[str, Any]
