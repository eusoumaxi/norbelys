---
title: Privacy Policy
tab: Privacy
order: 1
summary: What we collect, why, where it lives, who else touches it and how to make us delete it.
description: How Xuxil, Inc. collects, uses, shares and protects personal data when you visit norbelys.com or use Norbelys, and the rights you have over it.
contact: privacy
revisions:
  - version: "1.0"
    effective: 2026-10-05
    summary: First version, published with Norbelys’s launch.
---

This policy explains how Xuxil, Inc. handles personal data when you visit norbelys.com, sign up for Norbelys or use it to email people. It speaks to two kinds of people: our customers, who use Norbelys, and the people our customers email.

## 1. Who we are

> We’re Xuxil, Inc., a Delaware company, and we make Norbelys. For your account we decide what happens to your data. For the people you email, you decide, and we follow your instructions.

Norbelys is made and run by Xuxil, Inc. (“Xuxil”, “we”, “us”), a Delaware corporation. You can reach us at [privacy@norbelys.com](mailto:privacy@norbelys.com) or at Xuxil, Inc., {{address}}.

We play two roles:

- **Controller** for data about our customers and the visitors to our website: your account, your billing details, how you use the dashboard and anything you send us.
- **Processor**, or service provider, for the data our customers put into Norbelys: their contacts, the emails they send and the replies they receive. The customer is the controller of that data, and our [Data Processing Addendum](/legal/dpa) governs how we handle it.

## 2. What we collect about customers

> Your name, email and workspace; your mailboxes’ connection details, sealed; billing details, through {{payments}}; and the records any service keeps to stay secure.

### 2.1 Your account

Your name, your email address, the workspaces you belong to and your role in each, the public keys of your passkeys and your preferences.

### 2.2 Your mailboxes

When you connect a mailbox, we keep what we need to send and read mail for you: the OAuth tokens Google or Microsoft give us, or the SMTP and IMAP credentials you enter. Each one is sealed with AES-256-GCM before it reaches our database.

### 2.3 Billing

Your billing contact, company name, address and tax ID, and your plan. Card details go straight to our payment processor, {{payments}}. We never see or store your full card number.

### 2.4 Usage and security records

Our servers keep short technical records of requests to Norbelys, such as the time, the action and its result. Audit logs record important actions in a workspace, such as who invited whom or who changed a setting. Where we keep an IP address, we keep it as a keyed hash, not as the address itself.

### 2.5 What you tell us

Your emails to our inboxes, support conversations and anything else you choose to send us.

## 3. What customers put in Norbelys

> The people you email, the emails you send and the replies you get. It’s your data. We use it only to run Norbelys for you.

Our customers upload the people they want to email, with their names, email addresses, companies, job titles and any other fields they add. They write emails and sequences and receive replies. Norbelys stores those people, the messages sent to them, the replies they send back, delivery events such as sent, bounced or unsubscribed and, only when the customer turns tracking on, opens and clicks.

We process all of it on our customer’s instructions, as the DPA describes. If you received an email sent with Norbelys and want to know what’s held about you, or want it deleted, see [section 9](#your-rights).

## 4. How we use personal data

> To run Norbelys, keep it safe, bill you, answer you and tell you what changes. Never to sell it, and never for ads.

We use personal data to:

- provide Norbelys: send your emails, read your replies, run your campaigns and show your reports;
- keep it secure: sign you in, stop abuse and fraud, and enforce our [Acceptable Use Policy](/legal/acceptable-use) and rate limits;
- bill you and keep the records the law requires;
- answer your messages and support you;
- tell you about changes to Norbelys, to your account and to these documents.

We don’t sell personal data. We don’t share it for advertising, we don’t build profiles for advertisers and we don’t use our customers’ contacts or emails to market Norbelys.

### 4.1 Legal bases

Where the GDPR or the UK GDPR applies, we rely on **contract** to provide Norbelys to you; on **legitimate interests** to secure it, prevent abuse and improve it, weighed against your rights; on **legal obligation** for tax and accounting records and for lawful requests; and on **consent** where we ask for it, which you can withdraw at any time.

## 5. AI

> AI writes first lines and sorts replies only where you switch it on. It sees as little as it needs, and nobody trains models on your data.

Norbelys uses AI for two jobs, both run on Anthropic’s Claude models:

- **First lines.** When a step uses Personalize with AI, the model receives the step’s instructions and only the fields of the person’s record that your workspace allows, plus, where the step asks for it, public facts about the person’s company, such as its website and recent news. It never receives the person’s email address. If what it writes contains a link, an email address or a phone number, Norbelys discards it and sends your own words instead.
- **Sorting replies.** Off until a workspace turns it on. Then the model receives the reply’s sender and subject, the few headers that tell an automatic reply from a person, and up to 2,000 characters of the reply, with earlier quoted messages cut and other email addresses, links and phone numbers masked.

Neither we nor Anthropic use your data to train AI models. For each AI request we keep the model, the tokens, the cost and the outcome. We never log the prompt or what the model wrote.

## 6. Who we share it with

> Only the companies that help us run Norbelys, which we list, the services you connect yourself, and the authorities when the law truly requires it.

- **Sub-processors.** Companies that host, secure or support Norbelys for us, under contracts that hold them to this policy: Hetzner for servers, Cloudflare for the network, storage and bot protection, Anthropic for AI, {{payments}} for payments and SigNoz for monitoring. The [sub-processors page](/legal/subprocessors) lists what each one does and where.
- **Services you connect.** When you connect Google Workspace, Microsoft 365, an SMTP server, a webhook endpoint or an AI assistant, we send data there because you asked us to. Their terms and policies apply to what they do with it.
- **The law.** We disclose data only when a valid legal process requires it. We push back on requests that go too far, and we tell the customer concerned first unless the law forbids it.
- **A change of ownership.** If Xuxil merges or is acquired, data passes to the new owner under this policy, and we’ll tell you before it does.

## 7. Where your data lives

> On servers in {{regions}}, behind Cloudflare’s network. Where data reaches the US, the EU’s approved contracts go with it.

Norbelys runs on Hetzner servers in {{regions}}. Files such as imports, exports, attachments and archives are kept in Cloudflare R2 storage in the EU. Cloudflare’s network carries traffic to and from Norbelys through its data centers around the world, the one closest to you first.

Xuxil is a US company, and some of our sub-processors are in the United States. When personal data from the European Economic Area, the United Kingdom or Switzerland reaches us or them, we rely on the European Commission’s Standard Contractual Clauses, with the UK Addendum and the Swiss amendments, or on the EU–US Data Privacy Framework where the recipient is certified under it.

## 8. How long we keep it

> As long as your workspace exists, then 30 days. A few things go sooner, and billing records stay longer because tax law says so.

| Data | How long we keep it |
| --- | --- |
| Account and workspace data | While the workspace exists, then 30 days |
| People, campaigns and replies | Until you delete them, or the workspace |
| Sent messages | 7 days in the live database, then in an encrypted archive for reports and exports, until the workspace is deleted |
| Open and click events | 30 days in the live database, then in the archive |
| Unsubscribes and bounces | Until the workspace is deleted, so nobody is emailed again by mistake |
| Webhook delivery history | 7 days |
| Audit logs | 180 days |
| Export files | 7 days; each download link lasts 15 minutes |
| Sign-in codes | Valid for 10 minutes, deleted a day after they expire |
| Ended sessions | 30 days |
| Backups | 30 days, then overwritten |
| Billing records | 7 years |

When a workspace owner deletes a workspace, every person, message, reply, file and credential in it is erased 30 days later. Copies in our backups are overwritten within the 30 days after that.

## 9. Your rights

> You can see, fix, export or delete your data, and object to how we use it. If someone emailed you through Norbelys, we’ll help you reach them.

Depending on where you live, you may have the right to access the personal data we hold about you, to correct it, to delete it, to receive it in a portable format, to restrict or object to how we use it and to withdraw your consent. Californians also have the right to know what we collect and disclose, to delete and correct it and not to be treated differently for asking. We don’t sell or share personal information, as California law defines those words.

- **If you use Norbelys,** most of this is in the dashboard: edit your profile, export people and their history as CSV or JSONL and delete a workspace. For anything else, write to [privacy@norbelys.com](mailto:privacy@norbelys.com).
- **If someone emailed you using Norbelys,** the sender controls that data. Reply to them, or use the unsubscribe link in the email: Norbelys honors it at once and for good. You can also write to [privacy@norbelys.com](mailto:privacy@norbelys.com); we’ll pass your request to the right customer and help them act on it.

We answer within 30 days, confirm who’s asking before we act and never charge for a reasonable request. You can also complain to your data protection authority.

## 10. Cookies

> norbelys.com sets no cookies at all. The dashboard sets two, both to keep you signed in safely.

This website, norbelys.com, sets no cookies, stores nothing in your browser and runs no analytics or advertising scripts.

The dashboard, app.norbelys.com, sets these:

| Cookie | What it’s for | How long |
| --- | --- | --- |
| `__Host-nb_session` | Keeps you signed in | 30 days of inactivity, 90 days at most |
| `__Host-nb_ceremony` | Ties a sign-in to the browser that started it | 10 minutes |

It also keeps a few interface preferences in your browser’s local storage, such as the last workspace you opened and whether the sidebar is collapsed. They never leave your device. When sign-in asks for a captcha, Cloudflare Turnstile checks that you’re a person.

## 11. Security

> Credentials sealed, workspaces walled off, every connection encrypted and every important action logged.

- Mailbox credentials, tokens and secrets are sealed with AES-256-GCM, with a separate key for each purpose.
- Row-level security in the database keeps each workspace apart from every other, and our own application can’t switch it off.
- Mail leaves only over encrypted connections, with STARTTLS required and never downgraded; the dashboard and the API answer only over HTTPS.
- You sign in with a passkey or a one-time email code, and a workspace can require single sign-on.
- API keys are stored only as hashes and shown once.
- Owners and admins can read a 180-day audit log of their workspace.

No system is perfectly secure. If a breach affects your data, we tell you without undue delay and within 48 hours of knowing, and we notify the data protection authority within 72 hours where the law requires it. To report a vulnerability, write to [security@norbelys.com](mailto:security@norbelys.com).

## 12. Children

> Norbelys is for businesses, not for children.

Norbelys is a business tool. It isn’t meant for anyone under 16, and we don’t knowingly collect their data. If you believe a child has given us personal data, write to [privacy@norbelys.com](mailto:privacy@norbelys.com) and we’ll delete it.

## 13. Changes to this policy

> We tell you before anything important changes, and we show exactly what changed.

Every change is posted on this page, and the record at the end shows the words we changed. For changes that matter, we email workspace owners at least 30 days before they take effect.

## 14. Contact

> One inbox, privacy@norbelys.com, and a person reads it.

Questions, requests and complaints go to [privacy@norbelys.com](mailto:privacy@norbelys.com), or by post to Xuxil, Inc., {{address}}.
