---
title: "The cold email deliverability checklist, in plain English"
description: "SPF, DKIM, DMARC, warm-up, daily limits and list hygiene, explained without the jargon. Run through it before your next campaign."
date: 2026-09-15
tag: Deliverability
cover:
  glyph: warmup
  tone: mist
---

No tool can promise your emails land in the inbox, and anyone who does is selling something. What you can do is stop giving mailbox providers reasons to send you to spam. This checklist covers the reasons that matter most.

## Set up your domain

Mailbox providers want to know that you are who you say you are. Three DNS records prove it.

**SPF** lists the servers allowed to send email for your domain. If you send through Google Workspace, it looks like this:

```txt
v=spf1 include:_spf.google.com ~all
```

**DKIM** signs every email with a key only your domain has. Your email provider generates the record; you paste it into your DNS.

**DMARC** tells providers what to do when an email fails those checks, and where to send reports. Start by monitoring:

```txt
v=DMARC1; p=none; rua=mailto:dmarc@yourdomain.com
```

Google and Yahoo now expect bulk senders to have all three, along with a one-click unsubscribe and a low spam complaint rate. Even if you send far less than that, you want to look like a sender who follows the rules.

## Protect your main domain

Many teams send cold email from a second domain, like `getacme.com` instead of `acme.com`, so a bad week of outreach never touches the domain your invoices and support emails come from. Point the second domain's website at your main one so it still looks legitimate.

Use a custom tracking domain for opens and clicks, too. Shared tracking domains are used by thousands of senders, and you inherit their reputation.

## Warm up new inboxes

A brand-new inbox that sends a hundred emails on day one looks exactly like a spammer. Start with a handful a day and grow slowly over a few weeks. Norbelys ramps new inboxes up gradually and caps each one per day, so you can't skip this by accident.

## Send like a person

- **Keep a daily limit for every inbox.** Spread a big list over more inboxes, not more emails per inbox.
- **Send during business hours** in your prospect's time zone, with a gap between emails.
- **Keep the first email plain.** No attachments, few links, no images. A cold email that looks like a newsletter gets treated like one.

## Clean your list before you send

Bounces are one of the loudest signals you can send a mailbox provider.

- **Verify addresses** before a campaign, and drop the ones that don't exist.
- **Remove hard bounces** right away, and never send to them again.
- **Honor every unsubscribe** across every campaign, not just the one where they clicked.

## Keep an eye on the numbers

Watch your bounce rate, your spam complaints and your reply rate. Google Postmaster Tools shows how Gmail sees your domain. If replies drop and bounces climb, slow down before you scale up.
