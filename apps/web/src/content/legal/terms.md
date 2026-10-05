---
title: Terms of Service
tab: Terms
order: 2
summary: The agreement between you and Xuxil, Inc. for using Norbelys, from the dashboard to the API.
description: The Terms of Service between Xuxil, Inc. and the people and companies that use Norbelys, its API, webhooks, MCP server and CLI.
contact: legal
revisions:
  - version: "1.0"
    effective: 2026-10-05
    summary: First version, published with Norbelys’s launch.
---

These terms are the agreement between you and Xuxil, Inc. for using Norbelys. Please read them. The short version beside each section is there to help; the section itself is what counts.

## 1. The agreement

> By using Norbelys you agree to these terms. If you use it for a company, you agree for the company.

These Terms of Service (“Terms”) are an agreement between Xuxil, Inc., a Delaware corporation (“Xuxil”, “we”, “us”), and the person or organization that uses Norbelys (“you”). They cover the dashboard at app.norbelys.com, the API, the SDKs, webhooks, the MCP server, the CLI and everything else we provide under the Norbelys name (together, “Norbelys”).

Our [Privacy Policy](/legal/privacy), [Acceptable Use Policy](/legal/acceptable-use) and [Data Processing Addendum](/legal/dpa) are part of these Terms. Where they disagree about personal data, the DPA wins.

If you accept these Terms for a company or another organization, you confirm that you may bind it, and “you” means that organization.

## 2. Your account and workspaces

> Keep your sign-in safe. A workspace belongs to its owner, who decides who gets in and answers for what happens inside.

You must be at least 18 and use Norbelys for your business or work, not as a consumer. Give us accurate information and keep it up to date.

Norbelys is organized in workspaces. Each workspace has an owner, who controls it and can invite others as admins, members or viewers. You’re responsible for everything done in your workspace, including by the people you invite, by your API keys and by the apps and assistants you connect.

Keep your passkeys, sign-in codes and API keys to yourself. If you think someone else is using them, tell us at once at [security@norbelys.com](mailto:security@norbelys.com).

## 3. Your mailboxes and what you send

> You send from your own mailboxes, so the emails are yours, and so is the responsibility for them.

Norbelys sends email from the mailboxes and relays you connect. You confirm you have the right to use each mailbox, each domain and each address you send from.

You are the sender of every email sent from your workspace. You are responsible for what it says, who receives it and for following the laws that apply to it, such as CAN-SPAM, the GDPR and the ePrivacy rules, CASL and the rules of your mailbox providers, including Google’s and Microsoft’s.

Norbelys gives you tools to send responsibly: an unsubscribe link and header in every campaign email, suppression lists that never forget, daily limits, a gap between emails and stop-on-reply. Don’t remove, hide or work around them. We may slow or pause sending from a workspace to protect the people receiving it, your domains or our service, and we’ll tell you why.

No one can guarantee that an email reaches an inbox, and we don’t.

## 4. Acceptable use

> No spam, no deception, no harm. The Acceptable Use Policy has the details.

You must follow our [Acceptable Use Policy](/legal/acceptable-use). It says who you may email, what you may send and how. Section 13 explains what happens if it’s broken.

## 5. Your content and your data

> What you put in Norbelys stays yours. You give us only the rights we need to run Norbelys for you.

Everything you or your workspace put into Norbelys, including people, lists, templates, emails, replies and files (“Customer Data”), belongs to you.

You give us a worldwide, non-exclusive license to host, copy, process, transmit and display Customer Data only to provide, secure and support Norbelys for you, as these Terms and the DPA describe, and as the law requires. The license ends when the data is deleted.

You confirm that you have the rights, notices and legal basis needed to give us Customer Data and to email the people in it.

You can export people and their history whenever you like, as CSV or JSONL.

If you send us ideas or feedback, we may use them freely and without paying you, and you keep any rights you had in them.

## 6. AI features

> AI is optional, works only from what you allow and you check what it writes. Nobody trains models on your data.

Norbelys can write a personalized first line for a step and sort the replies you receive. Both are optional: first lines run only on the steps where you turn them on, and reply sorting is off until you turn it on.

AI can be wrong. You are responsible for the emails you send and for any decision you make from a sorted reply. Norbelys follows fixed rules to reduce mistakes: it sends your own words when a first line breaks them, it never lets AI decide bounces, complaints or unsubscribes, and your corrections always win.

Each workspace has a monthly AI allowance included in its plan. When it runs out, AI pauses until the next month and Norbelys sends your own words instead.

Neither we nor our AI provider use Customer Data to train AI models.

## 7. The API, webhooks, MCP and CLI

> Build on Norbelys as much as you like. Keep your keys secret, stay within the limits and don’t use it to harm anyone.

You may use the API, the SDKs, webhooks, the MCP server and the CLI to build on Norbelys, for your own workspaces or for your clients’.

- **Keys.** API keys act for your workspace, with no more access than the role of the person who made them. Keep them secret. We store only their hashes and show each one once.
- **Limits.** Each workspace may make up to 6,000 API requests a minute. We may change limits to protect the service, and we’ll announce lasting changes in advance.
- **Webhooks.** Verify the signature of every delivery. We retry a failed delivery for about three days and switch an endpoint off when it answers 410.
- **Assistants.** An AI assistant connected through the MCP server acts for you, with the access you grant on the consent screen. You are responsible for what it does, and you can revoke a grant at any time under Connected apps.

Don’t use them to get around these Terms, the Acceptable Use Policy or our limits, to collect email addresses or to disrupt Norbelys or anyone using it.

## 8. Open source and self-hosting

> The code is Apache-2.0. These terms cover our hosted service; the license covers the code. The name and logo stay ours.

The source code of Norbelys is published at [github.com/eusoumaxi/norbelys](https://github.com/eusoumaxi/norbelys) under the Apache License 2.0. That license, not these Terms, governs what you do with the code.

If you run Norbelys yourself, you run it: we don’t host, access or support your installation unless we agree to in writing, and these Terms don’t apply to it.

“Norbelys” and its logos are trademarks of Xuxil and are not licensed under the Apache License. Don’t use them to name your fork or your service, or in a way that suggests we made or endorse it.

## 9. Fees and payment

> One plan, everything included. You pay monthly in advance and can cancel whenever you like.

Norbelys costs the price shown on norbelys.com when you subscribe. Today that is launch pricing from $20 a month, with every feature included. Fees are charged monthly in advance through our payment processor, {{payments}}, and exclude taxes, which we add where the law requires.

We’ll email workspace owners at least 30 days before a price change. It applies from your next billing period, so you can cancel before it does.

If a payment fails, we’ll tell you and try again. If a workspace stays unpaid for 14 days, we may pause its sending; your data stays where it is.

Fees already paid are not refunded, except where the law requires it. If something went wrong on our side, write to us; we’re reasonable.

## 10. Cancelling and leaving

> Leave whenever you want. Export first: 30 days after a workspace is deleted, it’s gone for good.

You can cancel at any time from the dashboard or by writing to [support@norbelys.com](mailto:support@norbelys.com). Cancellation takes effect at the end of the period you’ve paid for.

An owner can delete a workspace whenever they like. Its credentials stop working right away, and everything in it is erased 30 days later. Export anything you want to keep before then.

## 11. What we commit to

> We run Norbelys carefully, keep it secure, back it up and tell you when something breaks or changes.

We will provide Norbelys with reasonable skill and care, protect Customer Data with the measures in the DPA, keep backups and tell you about incidents and planned maintenance that affect you.

Norbelys keeps improving, so features change. If we remove a feature you rely on, we’ll tell workspace owners at least 30 days in advance, unless we must act sooner for security or legal reasons.

## 12. Confidentiality

> What you show us in confidence stays between us, and the same goes the other way.

Each of us may see the other’s confidential information, such as Customer Data, security details or unpublished plans. Each of us will use the other’s confidential information only for this agreement, protect it at least as carefully as our own and share it only with people who need it and are bound to keep it confidential. This doesn’t cover information that is public, already known to the receiver or independently developed, or that the law requires us to disclose.

## 13. Suspension and termination

> We only suspend to stop harm, and we tell you why. Serious or repeated abuse ends the account.

We may suspend sending or access to a workspace, at once and without notice where needed, to stop serious harm: spam or complaints, a security risk, a breach of the Acceptable Use Policy, an unpaid account or a legal requirement. We’ll tell you why, and lift the suspension once the cause is fixed.

Either of us may end this agreement if the other materially breaches it and doesn’t fix the breach within 15 days of being told. We may end it at once for a serious or repeated breach of the Acceptable Use Policy.

When the agreement ends, you may export your data for 30 days, unless the law or a serious abuse prevents it. Then we delete it, as the DPA describes. Sections 5, 12, 14, 15, 16 and 18 to 20 continue after the agreement ends.

## 14. Warranties and disclaimers

> Norbelys is provided as it is. We work hard on it, but we can’t promise perfect delivery or uptime.

Apart from the commitments in these Terms, Norbelys is provided “as is” and “as available”. To the extent the law allows, we disclaim every other warranty, express or implied, including merchantability, fitness for a particular purpose and non-infringement. We don’t promise that Norbelys will be uninterrupted or error-free, that any email will be delivered or read, or that any campaign will get a reply.

## 15. Limitation of liability

> If things go wrong, neither of us owes the other more than what you paid us in the last 12 months.

To the extent the law allows, neither of us is liable for indirect, incidental, special, consequential or punitive damages, or for lost profits, revenue, goodwill or data, even if warned of them.

Each party’s total liability arising from this agreement is limited to the amounts you paid us for Norbelys in the 12 months before the event that gave rise to the claim, or $100 if that is more.

These limits don’t apply to your payment obligations, to either party’s obligations under section 16, to a breach of section 12 or to liability that the law doesn’t allow to be limited.

## 16. Indemnity

> If someone sues us over emails you sent, you cover it. If someone claims Norbelys itself infringes their rights, we cover it.

You will defend Xuxil against any third-party claim arising from Customer Data, from the emails sent from your workspace or from your breach of these Terms, the Acceptable Use Policy or the law, and pay the damages, costs and reasonable legal fees that result.

We will defend you against any third-party claim that the hosted Norbelys service, as we provide it, infringes that party’s intellectual property rights, and pay the damages, costs and reasonable legal fees that result. This doesn’t cover claims caused by Customer Data, by changes or combinations we didn’t make, or by self-hosted installations.

The party asking for defense must tell the other promptly, let it control the defense and help reasonably.

## 17. Changes to these terms

> You get 30 days’ notice before a change that matters, and the record shows exactly what changed.

We may update these Terms. Every change is posted on this page, and the record at the end shows the words we changed. For a change that matters, we email workspace owners at least 30 days before it takes effect. If you don’t agree, you can cancel before then; if you keep using Norbelys afterwards, the new Terms apply.

## 18. Governing law and disputes

> Delaware law applies. Before anyone goes to court, we talk, and most things get solved by email.

These Terms are governed by the laws of the State of Delaware, without regard to its conflict-of-laws rules. Before starting proceedings, write to [legal@norbelys.com](mailto:legal@norbelys.com) and give us 30 days to resolve the dispute in good faith. Any proceedings must be brought in the state or federal courts located in New Castle County, Delaware, and both of us agree to their jurisdiction. Either of us may still ask any court for urgent relief to protect its rights.

## 19. General

> The usual fine print: how notices travel, who can take over the agreement and what happens if one sentence turns out to be invalid.

- **Notices.** We send notices to the email address of your workspace owner. Send notices to us at [legal@norbelys.com](mailto:legal@norbelys.com), or by post to Xuxil, Inc., {{address}}.
- **Assignment.** Neither of us may transfer this agreement without the other’s consent, except to a successor in a merger, acquisition or sale of most of its assets.
- **Force majeure.** Neither of us is responsible for delays caused by events beyond reasonable control, such as natural disasters, wars, network failures beyond our providers or acts of government.
- **Compliance.** You will comply with export controls and sanctions laws, and won’t use Norbelys from or for a sanctioned country or person.
- **Severability and waiver.** If a provision is unenforceable, the rest still applies. Not enforcing a right isn’t giving it up.
- **Entire agreement.** These Terms, with the documents in section 1, are the entire agreement between us about Norbelys and replace any earlier ones.
- **Independence.** We are independent contractors. Nothing here creates a partnership, employment or agency.

## 20. Contact

> Questions about these terms go to legal@norbelys.com.

Write to [legal@norbelys.com](mailto:legal@norbelys.com), or by post to Xuxil, Inc., {{address}}.
