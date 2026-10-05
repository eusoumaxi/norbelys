//! Real-world shaped messages for the parser tests: delivery status notifications as Postfix,
//! Gmail and Exchange Online write them (RFC 3464), abuse reports as RFC 5965 and feedback loops
//! write them, and an ordinary reply. Addresses use reserved example domains.

/// The simple DSN of RFC 3464 §10.1 (a sendmail report of a delay that became a failure), with
/// its returned message given headers so the original `Message-ID` can be found.
pub(crate) const RFC_3464: &str = "Date: Thu, 7 Jul 1994 17:16:05 -0400\r
From: Mail Delivery Subsystem <MAILER-DAEMON@CS.UTK.EDU>\r
Message-Id: <199407072116.RAA14128@CS.UTK.EDU>\r
Subject: Returned mail: Cannot send message for 5 days\r
To: <owner-ups-mib@CS.UTK.EDU>\r
MIME-Version: 1.0\r
Content-Type: multipart/report; report-type=delivery-status;\r
    boundary=\"RAA14128.773615765/CS.UTK.EDU\"\r
\r
--RAA14128.773615765/CS.UTK.EDU\r
\r
   The original message was received at Sat, 2 Jul 1994 17:10:28 -0400\r
   from root@localhost\r
\r
      ----- The following addresses had delivery problems -----\r
<louisl@larry.slip.umd.edu>  (unrecoverable error)\r
\r
--RAA14128.773615765/CS.UTK.EDU\r
content-type: message/delivery-status\r
\r
Reporting-MTA: dns; cs.utk.edu\r
\r
Original-Recipient: rfc822;louisl@larry.slip.umd.edu\r
Final-Recipient: rfc822;louisl@larry.slip.umd.edu\r
Action: failed\r
Status: 4.0.0\r
Diagnostic-Code: smtp; 426 connection timed out\r
Last-Attempt-Date: Thu, 7 Jul 1994 17:15:49 -0400\r
\r
--RAA14128.773615765/CS.UTK.EDU\r
content-type: message/rfc822\r
\r
From: owner-ups-mib@CS.UTK.EDU\r
To: louisl@larry.slip.umd.edu\r
Subject: ups-mib\r
Message-ID: <199407021710.RAA0001@CS.UTK.EDU>\r
\r
original message\r
--RAA14128.773615765/CS.UTK.EDU--\r
";

/// Gmail's "Address not found" bounce: the diagnostic folded over three lines, the returned
/// message whole.
pub(crate) const GMAIL_BOUNCE: &str = "Return-Path: <>\r
From: Mail Delivery Subsystem <mailer-daemon@googlemail.com>\r
To: ada@example.com\r
Subject: Delivery Status Notification (Failure)\r
Message-ID: <5f1a2b3c.abc@mx.google.com>\r
Date: Wed, 01 Oct 2026 10:00:05 -0700 (PDT)\r
MIME-Version: 1.0\r
Content-Type: multipart/report; boundary=\"0000000000001234\"; report-type=delivery-status\r
\r
--0000000000001234\r
Content-Type: text/plain; charset=\"UTF-8\"\r
\r
** Address not found **\r
\r
Your message wasn't delivered to ghost@example.org because the address couldn't be found.\r
\r
--0000000000001234\r
Content-Type: message/delivery-status\r
\r
Reporting-MTA: dns; googlemail.com\r
Received-From-MTA: dns; ada@example.com\r
Arrival-Date: Wed, 01 Oct 2026 10:00:04 -0700 (PDT)\r
\r
Final-Recipient: rfc822; ghost@example.org\r
Action: failed\r
Status: 5.1.1\r
Remote-MTA: dns; mx.example.org. (192.0.2.1, the server for the domain example.org.)\r
Diagnostic-Code: smtp; 550-5.1.1 The email account that you tried to reach does not exist.\r
 Please try double-checking the recipient's email address for typos or\r
 unnecessary spaces. https://support.google.com/mail/?p=NoSuchUser\r
Last-Attempt-Date: Wed, 01 Oct 2026 10:00:05 -0700 (PDT)\r
\r
--0000000000001234\r
Content-Type: message/rfc822\r
\r
From: Ada <ada@example.com>\r
To: ghost@example.org\r
Subject: Quick question\r
Message-ID: <m1.t1.tag@mail.example.com>\r
Date: Wed, 01 Oct 2026 17:00:00 +0000\r
\r
Hello\r
--0000000000001234--\r
";

/// Exchange Online's non-delivery report for two recipients (a full mailbox and an unknown
/// address), returning only the original headers.
pub(crate) const EXCHANGE_NDR: &str = "From: Microsoft Outlook <MicrosoftExchange329e71ec88ae4615bbc36ab6ce41109e@contoso.onmicrosoft.com>\r
To: <ada@example.com>\r
Subject: Undeliverable: Quick question\r
Date: Wed, 1 Oct 2026 10:00:01 +0000\r
MIME-Version: 1.0\r
Content-Type: multipart/report; report-type=delivery-status;\r
\tboundary=\"b_ndr\"\r
\r
--b_ndr\r
Content-Type: text/plain; charset=\"us-ascii\"\r
\r
Delivery has failed to these recipients or groups:\r
\r
--b_ndr\r
Content-Type: message/delivery-status\r
\r
Reporting-MTA: dns;AM0PR01MB1234.eurprd01.prod.exchangelabs.com\r
Received-From-MTA: dns;AM0PR01MB5678.eurprd01.prod.exchangelabs.com\r
Arrival-Date: Wed, 1 Oct 2026 10:00:00 +0000\r
\r
Final-Recipient: rfc822;full@contoso.com\r
Action: failed\r
Status: 5.2.2\r
Diagnostic-Code: smtp;554 5.2.2 mailbox full; STOREDRV.Deliver.Exception:QuotaExceededException.MapiExceptionShutoffQuotaExceeded\r
Remote-MTA: dns;AM0PR01MB9999.eurprd01.prod.exchangelabs.com\r
\r
Final-Recipient: rfc822;gone@contoso.com\r
Action: failed\r
Status: 5.1.10\r
Diagnostic-Code: smtp;550 5.1.10 RESOLVER.ADR.RecipientNotFound; Recipient not found by SMTP address lookup\r
\r
--b_ndr\r
Content-Type: text/rfc822-headers\r
\r
From: Ada <ada@example.com>\r
To: full@contoso.com, gone@contoso.com\r
Subject: Quick question\r
Message-ID: <m2.t1.tag@mail.example.com>\r
Date: Wed, 1 Oct 2026 09:59:58 +0000\r
\r
--b_ndr--\r
";

/// Postfix's delay warning: `Action: delayed`, the message still queued.
pub(crate) const POSTFIX_DELAY: &str = "From: MAILER-DAEMON@mail.example.net (Mail Delivery System)\r
Subject: Delayed Mail (still being retried)\r
To: ada@example.com\r
MIME-Version: 1.0\r
Content-Type: multipart/report; report-type=delivery-status;\r
\tboundary=\"8A1B2C3D4E.1696154400/mail.example.net\"\r
\r
--8A1B2C3D4E.1696154400/mail.example.net\r
Content-Description: Notification\r
Content-Type: text/plain; charset=us-ascii\r
\r
This is the mail system at host mail.example.net.\r
\r
--8A1B2C3D4E.1696154400/mail.example.net\r
Content-Description: Delivery report\r
Content-Type: message/delivery-status\r
\r
Reporting-MTA: dns; mail.example.net\r
X-Postfix-Queue-ID: 8A1B2C3D4E\r
Arrival-Date: Wed,  1 Oct 2026 06:00:00 +0000 (UTC)\r
\r
Final-Recipient: rfc822; slow@example.org\r
Original-Recipient: rfc822;slow@example.org\r
Action: delayed\r
Status: 4.4.7\r
Diagnostic-Code: X-Postfix; delivery temporarily suspended: connect to mx.example.org[192.0.2.7]:25: Connection timed out\r
Will-Retry-Until: Sun, 5 Oct 2026 06:00:00 +0000 (UTC)\r
\r
--8A1B2C3D4E.1696154400/mail.example.net\r
Content-Description: Undelivered Message Headers\r
Content-Type: text/rfc822-headers\r
\r
Message-ID: <m3.t2.tag@mail.example.com>\r
From: Ada <ada@example.com>\r
\r
--8A1B2C3D4E.1696154400/mail.example.net--\r
";

/// The simple abuse report of RFC 5965 Appendix B.1 (its spam's `Message-ID` written without
/// angle brackets, as in the RFC).
pub(crate) const RFC_5965: &str = "From: <abusedesk@example.com>\r
Date: Thu, 8 Mar 2005 17:40:36 EDT\r
Subject: FW: Earn money\r
To: <abuse@example.net>\r
MIME-Version: 1.0\r
Content-Type: multipart/report; report-type=feedback-report;\r
     boundary=\"part1_13d.2e68ed54_boundary\"\r
\r
--part1_13d.2e68ed54_boundary\r
Content-Type: text/plain; charset=\"US-ASCII\"\r
Content-Transfer-Encoding: 7bit\r
\r
This is an email abuse report for an email message received from IP\r
192.0.2.1 on Thu, 8 Mar 2005 14:00:00 EDT.\r
\r
--part1_13d.2e68ed54_boundary\r
Content-Type: message/feedback-report\r
\r
Feedback-Type: abuse\r
User-Agent: SomeGenerator/1.0\r
Version: 1\r
\r
--part1_13d.2e68ed54_boundary\r
Content-Type: message/rfc822\r
Content-Disposition: inline\r
\r
Received: from mailserver.example.net\r
        (mailserver.example.net [192.0.2.1])\r
        by example.com with ESMTP id M63d4137594e46;\r
        Thu, 08 Mar 2005 14:00:00 -0400\r
From: <somespammer@example.net>\r
To: <Undisclosed Recipients>\r
Subject: Earn money\r
MIME-Version: 1.0\r
Content-type: text/plain\r
Message-ID: 8787KJKJ3K4J3K4J3K4J3.mail@example.net\r
Date: Thu, 02 Sep 2004 12:31:03 -0500\r
\r
Spam Spam Spam\r
--part1_13d.2e68ed54_boundary--\r
";

/// A feedback-loop complaint as Yahoo's writes it: the original headers only, with a
/// `Feedback-ID`, the original recipient and the reported domain.
pub(crate) const FBL_COMPLAINT: &str = "From: staff@hotmail.example\r
To: fbl@mail.example.com\r
Subject: complaint about message from 192.0.2.25\r
MIME-Version: 1.0\r
Content-Type: multipart/report; report-type=feedback-report; boundary=\"arf\"\r
\r
--arf\r
Content-Type: text/plain\r
\r
This is a spam complaint.\r
\r
--arf\r
Content-Type: message/feedback-report\r
\r
Feedback-Type: abuse\r
User-Agent: Yahoo!-Mail-Feedback/2.0\r
Version: 0.1\r
Original-Mail-From: <bounces+m1@mail.example.com>\r
Original-Rcpt-To: <grace@yahoo.example>\r
Received-Date: Wed, 1 Oct 2026 10:00:00 +0000\r
Reported-Domain: example.com\r
Source-IP: 192.0.2.25\r
\r
--arf\r
Content-Type: text/rfc822-headers\r
\r
From: Ada <ada@example.com>\r
To: grace@yahoo.example\r
Subject: Quick question\r
Message-ID: <m1.t1.tag@mail.example.com>\r
Feedback-ID: m1:campaign7:norbelys:esp\r
\r
--arf--\r
";

/// A person's reply to a follow-up: folded `References`, an encoded subject, a text and an HTML
/// version.
pub(crate) const HUMAN_REPLY: &str = "From: \"Grace Hopper\" <grace@example.org>\r
To: Ada <ada@example.com>\r
Subject: =?UTF-8?B?UmU6IFF1aWNrIHF1ZXN0aW9uIOKAlCB0aGFua3M=?=\r
Date: Wed, 01 Oct 2026 12:30:00 +0000\r
Message-ID: <CAF=reply-1@mail.example.org>\r
In-Reply-To: <m2.t1.tag@mail.example.com>\r
References: <m1.t1.tag@mail.example.com>\r
 <m2.t1.tag@mail.example.com>\r
MIME-Version: 1.0\r
Content-Type: multipart/alternative; boundary=\"alt\"\r
\r
--alt\r
Content-Type: text/plain; charset=UTF-8\r
\r
Sounds good, let's talk Friday.\r
\r
> earlier text\r
--alt\r
Content-Type: text/html; charset=UTF-8\r
\r
<p>Sounds good, let's talk Friday.</p>\r
--alt--\r
";
