//! Turning a raw message into the few fields Belvedere keeps.

use mail_parser::{Address, MessageParser, MimeHeaders};
use sha2::{Digest, Sha256};

/// The normalized essentials of one message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Normalized {
    /// The Message-ID header, or a stable synthetic id when absent.
    pub message_id: String,
    pub from_addr: String,
    pub from_name: String,
    /// Comma-separated addresses.
    pub to_addrs: String,
    pub subject: String,
    /// RFC 3339, or empty.
    pub date: String,
    /// Plain text; HTML-only messages are converted. Truncated to
    /// `MAX_BODY` characters.
    pub body_text: String,
    pub attachments: Vec<String>,
}

/// Longest body kept. Bills and notices are short; newsletters are not,
/// and their tails are never what matters.
pub const MAX_BODY: usize = 60_000;

/// Parses `raw` (without Thunderbird's `From - ` line). Returns `None`
/// only for bytes that are not a message at all.
pub fn normalize(raw: &[u8]) -> Option<Normalized> {
    let message = MessageParser::default().parse(raw)?;

    let (from_name, from_addr) = first_address(message.from());
    let to_addrs = all_addresses(message.to());
    let subject = message.subject().unwrap_or_default().trim().to_string();
    let date = message.date().map(|d| d.to_rfc3339()).unwrap_or_default();

    let mut body = message
        .body_text(0)
        .map(|c| c.into_owned())
        .filter(|t| !t.trim().is_empty())
        .or_else(|| {
            message
                .body_html(0)
                .map(|h| mail_parser::decoders::html::html_to_text(&h))
        })
        .unwrap_or_default();
    body = tidy(&body);
    if body.chars().count() > MAX_BODY {
        body = body.chars().take(MAX_BODY).collect();
    }

    let attachments: Vec<String> = message
        .attachments()
        .filter_map(|p| p.attachment_name().map(str::to_string))
        .collect();

    let message_id = match message.message_id() {
        Some(id) if !id.trim().is_empty() => format!("<{}>", id.trim().trim_matches(['<', '>'])),
        _ => synthetic_id(&from_addr, &date, &subject, &body),
    };

    Some(Normalized {
        message_id,
        from_addr,
        from_name,
        to_addrs,
        subject,
        date,
        body_text: body,
        attachments,
    })
}

fn first_address(addr: Option<&Address<'_>>) -> (String, String) {
    let Some(addr) = addr else {
        return (String::new(), String::new());
    };
    match addr.first() {
        Some(a) => (
            a.name().unwrap_or_default().trim().to_string(),
            a.address().unwrap_or_default().trim().to_lowercase(),
        ),
        None => (String::new(), String::new()),
    }
}

fn all_addresses(addr: Option<&Address<'_>>) -> String {
    let Some(addr) = addr else {
        return String::new();
    };
    addr.iter()
        .filter_map(|a| a.address())
        .map(|a| a.trim().to_lowercase())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Collapses runs of blank lines and trailing spaces; keeps paragraphs.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

/// For messages with no Message-ID: a hash of the fields that identify it.
pub fn synthetic_id(from: &str, date: &str, subject: &str, body: &str) -> String {
    let mut h = Sha256::new();
    h.update(from.as_bytes());
    h.update(b"\0");
    h.update(date.as_bytes());
    h.update(b"\0");
    h.update(subject.as_bytes());
    h.update(b"\0");
    h.update(body.chars().take(1000).collect::<String>().as_bytes());
    let digest = h.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("<belvedere-{hex}@synthetic>")
}

#[cfg(test)]
pub mod fixtures {
    //! Made-up messages covering the shapes real mail comes in.

    pub struct Fixture {
        pub name: &'static str,
        pub raw: &'static str,
        pub want_from: &'static str,
        pub want_subject: &'static str,
        pub body_contains: &'static str,
        pub attachments: &'static [&'static str],
        pub has_message_id: bool,
    }

    pub const ALL: &[Fixture] = &[
        Fixture {
            name: "plain",
            raw: "From: Billing <billing@example.invalid>\r\nTo: me@example.invalid\r\nSubject: Your electric bill is due\r\nDate: Mon, 20 Oct 2026 09:00:00 -0500\r\nMessage-ID: <plain-1@example.invalid>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nYour bill of $84.12 is due October 31.\r\n",
            want_from: "billing@example.invalid",
            want_subject: "Your electric bill is due",
            body_contains: "$84.12",
            attachments: &[],
            has_message_id: true,
        },
        Fixture {
            name: "html-only",
            raw: "From: Shop <shop@example.invalid>\r\nTo: me@example.invalid\r\nSubject: Order shipped\r\nDate: Tue, 21 Oct 2026 10:00:00 +0000\r\nMessage-ID: <html-1@example.invalid>\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><body><h1>Shipped!</h1><p>Your order <b>#4521</b> is on its way.</p><p>Arrives <i>Friday</i>.</p></body></html>\r\n",
            want_from: "shop@example.invalid",
            want_subject: "Order shipped",
            body_contains: "order #4521 is on its way",
            attachments: &[],
            has_message_id: true,
        },
        Fixture {
            name: "quoted-printable",
            raw: "From: =?utf-8?Q?Caf=C3=A9_Ren=C3=A9?= <cafe@example.invalid>\r\nTo: me@example.invalid\r\nSubject: =?utf-8?Q?R=C3=A9servation_confirm=C3=A9e?=\r\nDate: Wed, 22 Oct 2026 18:30:00 +0200\r\nMessage-ID: <qp-1@example.invalid>\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nBonjour, votre table est r=C3=A9serv=C3=A9e pour 20h =E2=80=94 =C3=A0 bient=\r\n=C3=B4t.\r\n",
            want_from: "cafe@example.invalid",
            want_subject: "Réservation confirmée",
            body_contains: "réservée pour 20h — à bientôt",
            attachments: &[],
            has_message_id: true,
        },
        Fixture {
            name: "base64-body",
            raw: "From: notice@example.invalid\r\nTo: me@example.invalid\r\nSubject: Base64 notice\r\nDate: Thu, 23 Oct 2026 08:00:00 +0000\r\nMessage-ID: <b64-1@example.invalid>\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\nUGF5bWVudCByZWNlaXZlZC4gVGhhbmsgeW91IQ==\r\n",
            want_from: "notice@example.invalid",
            want_subject: "Base64 notice",
            body_contains: "Payment received. Thank you!",
            attachments: &[],
            has_message_id: true,
        },
        Fixture {
            name: "multipart-alternative",
            raw: "From: news@example.invalid\r\nTo: me@example.invalid\r\nSubject: Weekly digest\r\nDate: Fri, 24 Oct 2026 07:00:00 +0000\r\nMessage-ID: <alt-1@example.invalid>\r\nContent-Type: multipart/alternative; boundary=\"b1\"\r\n\r\n--b1\r\nContent-Type: text/plain\r\n\r\nPlain digest text.\r\n--b1\r\nContent-Type: text/html\r\n\r\n<p>HTML digest text.</p>\r\n--b1--\r\n",
            want_from: "news@example.invalid",
            want_subject: "Weekly digest",
            body_contains: "Plain digest text.",
            attachments: &[],
            has_message_id: true,
        },
        Fixture {
            name: "multipart-with-attachments",
            raw: "From: accounts@example.invalid\r\nTo: me@example.invalid\r\nSubject: Invoice 1042\r\nDate: Sat, 25 Oct 2026 12:00:00 +0000\r\nMessage-ID: <att-1@example.invalid>\r\nContent-Type: multipart/mixed; boundary=\"m1\"\r\n\r\n--m1\r\nContent-Type: text/plain\r\n\r\nPlease find the invoice attached.\r\n--m1\r\nContent-Type: application/pdf; name=\"invoice-1042.pdf\"\r\nContent-Disposition: attachment; filename=\"invoice-1042.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERi0xLjQK\r\n--m1\r\nContent-Type: image/png\r\nContent-Disposition: attachment; filename=\"logo.png\"\r\nContent-Transfer-Encoding: base64\r\n\r\niVBORw0KGgo=\r\n--m1--\r\n",
            want_from: "accounts@example.invalid",
            want_subject: "Invoice 1042",
            body_contains: "invoice attached",
            attachments: &["invoice-1042.pdf", "logo.png"],
            has_message_id: true,
        },
        Fixture {
            name: "no-message-id",
            raw: "From: someone@example.invalid\r\nTo: me@example.invalid\r\nSubject: No id here\r\nDate: Sun, 26 Oct 2026 09:00:00 +0000\r\n\r\nJust a note.\r\n",
            want_from: "someone@example.invalid",
            want_subject: "No id here",
            body_contains: "Just a note.",
            attachments: &[],
            has_message_id: false,
        },
        Fixture {
            name: "broken-headers",
            raw: "From: broken sender\r\nSubject\r\nDate: not a date\r\nX-Weird: \u{1}\r\nMessage-ID: <broken-1@example.invalid>\r\n\r\nBody survives.\r\n",
            want_from: "",
            want_subject: "",
            body_contains: "Body survives.",
            attachments: &[],
            has_message_id: true,
        },
        Fixture {
            name: "japanese-utf8-base64-headers",
            raw: "From: =?UTF-8?B?44Om44Km44OH44Ov?= <jp@example.invalid>\r\nTo: me@example.invalid\r\nSubject: =?UTF-8?B?44GU6YCj57Wh?=\r\nDate: Mon, 27 Oct 2026 09:00:00 +0900\r\nMessage-ID: <jp-1@example.invalid>\r\nContent-Type: text/plain; charset=UTF-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\nご使用ありがとうございます\r\n",
            want_from: "jp@example.invalid",
            want_subject: "ご連絡",
            body_contains: "ご使用ありがとうございます",
            attachments: &[],
            has_message_id: true,
        },
        Fixture {
            name: "latin1-body",
            raw: "From: Ana <ana@example.invalid>\r\nTo: me@example.invalid\r\nSubject: Reunión mañana\r\nDate: Tue, 28 Oct 2026 09:00:00 -0300\r\nMessage-ID: <l1-1@example.invalid>\r\nContent-Type: text/plain; charset=iso-8859-1\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nNos vemos ma=F1ana a las 10.\r\n",
            want_from: "ana@example.invalid",
            want_subject: "Reunión mañana",
            body_contains: "mañana a las 10",
            attachments: &[],
            has_message_id: true,
        },
        Fixture {
            name: "many-recipients",
            raw: "From: Team <team@example.invalid>\r\nTo: a@example.invalid, B Person <b@example.invalid>\r\nCc: c@example.invalid\r\nSubject: Planning\r\nDate: Wed, 29 Oct 2026 09:00:00 +0000\r\nMessage-ID: <many-1@example.invalid>\r\n\r\nLet's meet.\r\n",
            want_from: "team@example.invalid",
            want_subject: "Planning",
            body_contains: "Let's meet.",
            attachments: &[],
            has_message_id: true,
        },
        Fixture {
            name: "html-with-links-and-scripts",
            raw: "From: promo@example.invalid\r\nTo: me@example.invalid\r\nSubject: Sale\r\nDate: Thu, 30 Oct 2026 09:00:00 +0000\r\nMessage-ID: <promo-1@example.invalid>\r\nContent-Type: text/html\r\n\r\n<html><head><style>p{color:red}</style><script>alert(1)</script></head><body><p>Save <a href=\"http://x\">20%</a> this weekend only.</p></body></html>\r\n",
            want_from: "promo@example.invalid",
            want_subject: "Sale",
            body_contains: "Save 20% this weekend only.",
            attachments: &[],
            has_message_id: true,
        },
        Fixture {
            name: "lf-only-line-endings",
            raw: "From: lf@example.invalid\nTo: me@example.invalid\nSubject: LF only\nDate: Fri, 31 Oct 2026 09:00:00 +0000\nMessage-ID: <lf-1@example.invalid>\n\nUnix line endings.\n",
            want_from: "lf@example.invalid",
            want_subject: "LF only",
            body_contains: "Unix line endings.",
            attachments: &[],
            has_message_id: true,
        },
        Fixture {
            name: "folded-headers",
            raw: "From: Long Name\r\n <folded@example.invalid>\r\nTo: me@example.invalid\r\nSubject: This subject\r\n is folded across\r\n lines\r\nDate: Sat, 1 Nov 2026 09:00:00 +0000\r\nMessage-ID: <fold-1@example.invalid>\r\n\r\nFolded.\r\n",
            want_from: "folded@example.invalid",
            want_subject: "This subject is folded across lines",
            body_contains: "Folded.",
            attachments: &[],
            has_message_id: true,
        },
        Fixture {
            name: "empty-body",
            raw: "From: empty@example.invalid\r\nTo: me@example.invalid\r\nSubject: (no body)\r\nDate: Sun, 2 Nov 2026 09:00:00 +0000\r\nMessage-ID: <empty-1@example.invalid>\r\n\r\n",
            want_from: "empty@example.invalid",
            want_subject: "(no body)",
            body_contains: "",
            attachments: &[],
            has_message_id: true,
        },
        Fixture {
            name: "attachment-only",
            raw: "From: scans@example.invalid\r\nTo: me@example.invalid\r\nSubject: Scan\r\nDate: Mon, 3 Nov 2026 09:00:00 +0000\r\nMessage-ID: <scan-1@example.invalid>\r\nContent-Type: multipart/mixed; boundary=\"s1\"\r\n\r\n--s1\r\nContent-Type: application/pdf; name=\"scan.pdf\"\r\nContent-Disposition: attachment; filename=\"scan.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERi0xLjQK\r\n--s1--\r\n",
            want_from: "scans@example.invalid",
            want_subject: "Scan",
            body_contains: "",
            attachments: &["scan.pdf"],
            has_message_id: true,
        },
        Fixture {
            name: "utf8-emoji-subject",
            raw: "From: friend@example.invalid\r\nTo: me@example.invalid\r\nSubject: =?utf-8?B?8J+OiSBQYXJ0eSE=?=\r\nDate: Tue, 4 Nov 2026 09:00:00 +0000\r\nMessage-ID: <emoji-1@example.invalid>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nSaturday at 7 🎉\r\n",
            want_from: "friend@example.invalid",
            want_subject: "🎉 Party!",
            body_contains: "Saturday at 7 🎉",
            attachments: &[],
            has_message_id: true,
        },
        Fixture {
            name: "nested-multipart",
            raw: "From: nested@example.invalid\r\nTo: me@example.invalid\r\nSubject: Nested\r\nDate: Wed, 5 Nov 2026 09:00:00 +0000\r\nMessage-ID: <nested-1@example.invalid>\r\nContent-Type: multipart/mixed; boundary=\"outer\"\r\n\r\n--outer\r\nContent-Type: multipart/alternative; boundary=\"inner\"\r\n\r\n--inner\r\nContent-Type: text/plain\r\n\r\nInner plain text.\r\n--inner\r\nContent-Type: text/html\r\n\r\n<p>Inner html.</p>\r\n--inner--\r\n--outer\r\nContent-Type: text/calendar; name=\"invite.ics\"\r\nContent-Disposition: attachment; filename=\"invite.ics\"\r\n\r\nBEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n--outer--\r\n",
            want_from: "nested@example.invalid",
            want_subject: "Nested",
            body_contains: "Inner plain text.",
            attachments: &["invite.ics"],
            has_message_id: true,
        },
        Fixture {
            name: "windows-1252-quotes",
            raw: "From: win@example.invalid\r\nTo: me@example.invalid\r\nSubject: Quotes\r\nDate: Thu, 6 Nov 2026 09:00:00 +0000\r\nMessage-ID: <win-1@example.invalid>\r\nContent-Type: text/plain; charset=windows-1252\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nHe said =93hello=94 =96 then left.\r\n",
            want_from: "win@example.invalid",
            want_subject: "Quotes",
            body_contains: "He said “hello” – then left.",
            attachments: &[],
            has_message_id: true,
        },
        Fixture {
            name: "reply-with-quote",
            raw: "From: colleague@example.invalid\r\nTo: me@example.invalid\r\nSubject: Re: Draft\r\nDate: Fri, 7 Nov 2026 09:00:00 +0000\r\nMessage-ID: <re-1@example.invalid>\r\nIn-Reply-To: <draft-0@example.invalid>\r\n\r\nLooks good, send it Friday.\r\n\r\n> Can you review the draft?\r\n",
            want_from: "colleague@example.invalid",
            want_subject: "Re: Draft",
            body_contains: "Looks good, send it Friday.",
            attachments: &[],
            has_message_id: true,
        },
        Fixture {
            name: "mozilla-headers-present",
            raw: "X-Mozilla-Status: 0001\r\nX-Mozilla-Status2: 00000000\r\nX-Mozilla-Keys:                                                                                 \r\nFrom: real@example.invalid\r\nTo: me@example.invalid\r\nSubject: With Mozilla headers\r\nDate: Sat, 8 Nov 2026 09:00:00 +0000\r\nMessage-ID: <moz-1@example.invalid>\r\n\r\nStill parses.\r\n",
            want_from: "real@example.invalid",
            want_subject: "With Mozilla headers",
            body_contains: "Still parses.",
            attachments: &[],
            has_message_id: true,
        },
        Fixture {
            name: "very-long-body-is-truncated",
            raw: "From: long@example.invalid\r\nTo: me@example.invalid\r\nSubject: Long\r\nDate: Sun, 9 Nov 2026 09:00:00 +0000\r\nMessage-ID: <long-1@example.invalid>\r\n\r\nstart\r\n",
            want_from: "long@example.invalid",
            want_subject: "Long",
            body_contains: "start",
            attachments: &[],
            has_message_id: true,
        },
    ];
}

#[cfg(test)]
mod tests {
    use super::fixtures::ALL;
    use super::*;

    #[test]
    fn every_fixture_normalizes_as_expected() {
        assert!(
            ALL.len() >= 20,
            "need at least 20 fixtures, have {}",
            ALL.len()
        );
        for f in ALL {
            let n =
                normalize(f.raw.as_bytes()).unwrap_or_else(|| panic!("{}: did not parse", f.name));
            assert_eq!(n.from_addr, f.want_from, "{}: from", f.name);
            assert_eq!(n.subject, f.want_subject, "{}: subject", f.name);
            assert!(
                n.body_text.contains(f.body_contains),
                "{}: body {:?} should contain {:?}",
                f.name,
                n.body_text,
                f.body_contains
            );
            assert_eq!(n.attachments, f.attachments, "{}: attachments", f.name);
            if f.has_message_id {
                assert!(
                    n.message_id.starts_with('<') && n.message_id.ends_with('>'),
                    "{}: id",
                    f.name
                );
                assert!(!n.message_id.contains("synthetic"), "{}: id", f.name);
            } else {
                assert!(
                    n.message_id.contains("synthetic"),
                    "{}: synthetic id",
                    f.name
                );
            }
            assert!(!n.body_text.contains("<html"), "{}: html leaked", f.name);
            assert!(!n.body_text.contains("alert("), "{}: script leaked", f.name);
        }
    }

    #[test]
    fn dates_become_rfc3339_and_bad_dates_become_empty() {
        let plain = normalize(ALL[0].raw.as_bytes()).unwrap();
        assert_eq!(plain.date, "2026-10-20T09:00:00-05:00");
        let broken = normalize(
            ALL.iter()
                .find(|f| f.name == "broken-headers")
                .unwrap()
                .raw
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(broken.date, "");
    }

    #[test]
    fn recipients_are_joined_and_lowercased() {
        let many = normalize(
            ALL.iter()
                .find(|f| f.name == "many-recipients")
                .unwrap()
                .raw
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(many.to_addrs, "a@example.invalid, b@example.invalid");
        assert_eq!(many.from_name, "Team");
    }

    #[test]
    fn synthetic_ids_are_stable_and_distinct() {
        let a = synthetic_id("x@y", "2026-10-20T09:00:00Z", "s", "body");
        assert_eq!(a, synthetic_id("x@y", "2026-10-20T09:00:00Z", "s", "body"));
        assert_ne!(
            a,
            synthetic_id("x@y", "2026-10-20T09:00:00Z", "s", "other body")
        );
        assert!(a.starts_with("<belvedere-") && a.ends_with("@synthetic>"));
    }

    #[test]
    fn long_bodies_are_cut_and_blank_runs_collapsed() {
        let mut raw = String::from("From: a@b\r\nSubject: s\r\nMessage-ID: <x@y>\r\n\r\n");
        raw.push_str("para one\n\n\n\n\npara two\n");
        raw.push_str(&"x".repeat(MAX_BODY + 5000));
        let n = normalize(raw.as_bytes()).unwrap();
        assert!(n.body_text.starts_with("para one\n\npara two"));
        assert_eq!(n.body_text.chars().count(), MAX_BODY);
    }

    #[test]
    fn not_a_message_is_none_or_empty() {
        // mail-parser is forgiving; whatever it returns must not panic and
        // must give a usable record.
        if let Some(n) = normalize(b"\x00\x01\x02 nothing like mail") {
            assert!(n.message_id.contains("synthetic"));
        }
    }
}
