---
title: Sub-processors
tab: Sub-processors
order: 5
summary: The companies that help us run Norbelys, what each one does with your data and where.
description: The sub-processors Xuxil, Inc. uses to run the hosted Norbelys service, what each one processes and where, and how we announce changes.
contact: privacy
revisions:
  - version: "1.0"
    effective: 2026-10-05
    summary: First version, published with Norbelys’s launch.
---

These are the companies that process personal data for us to run the hosted Norbelys service. Each one is bound by a written contract with data protection terms at least as protective as our [Data Processing Addendum](/legal/dpa). If you run Norbelys yourself, none of them is involved unless you choose them.

## 1. Who they are

> Five companies, each with one job. Your workspace’s data is stored in the EU.

| Sub-processor | What it does for Norbelys | Data it handles | Where |
| --- | --- | --- | --- |
| Hetzner Online GmbH | The servers, databases and backups the whole service runs on | Everything in Norbelys | {{regions}} |
| Cloudflare, Inc. | DNS, network protection, encryption in transit, the dashboard’s hosting, file storage in R2 and the Turnstile captcha | Request details such as IP addresses; stored files such as imports, exports, attachments and archives | Storage in the EU; network worldwide |
| Anthropic, PBC | AI first lines and reply sorting, only where a workspace turns them on | The fields a workspace allows; reply excerpts with addresses, links and phone numbers masked | United States |
| {{paymentsCompany}} | Payments, invoices and tax | Billing contact, company details and payment method | United States |
| SigNoz, Inc. | Monitoring errors and performance | Technical details of requests; never email content or AI prompts | European Union |

## 2. What isn’t on the list

> The mailboxes, relays and apps you connect are your providers, not ours.

When you connect a mailbox from Google Workspace or Microsoft 365, any SMTP or IMAP server, a relay such as Amazon SES, SendGrid or Mailgun, a webhook endpoint or an AI assistant, Norbelys sends data there because you asked it to. Those providers process the data under your own agreement with them, so they are not our sub-processors.

## 3. Changes, and how to object

> We email workspace owners 30 days before a new sub-processor starts, and you can object.

Before a new sub-processor starts processing personal data for us, we update this page and email workspace owners at least 30 days in advance. To object on reasonable data protection grounds, write to [privacy@norbelys.com](mailto:privacy@norbelys.com) within those 30 days; section 5 of the [DPA](/legal/dpa#sub-processors) explains what happens next.
