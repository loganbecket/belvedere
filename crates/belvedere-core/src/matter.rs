//! Telling whether a new email is about something already on the list (a
//! reminder, a final notice, an updated amount) or about a new matter
//! (next month's bill). Pure logic over a few facts, so the pipeline and
//! the eval harness decide the same way.

use chrono::NaiveDate;

/// Due dates this close together belong to the same matter when the
/// sender and kind match; a monthly bill lands about 30 days apart.
pub const SAME_DUE_WITHIN_DAYS: i64 = 21;

/// Without due dates to compare, emails this far apart are assumed to be
/// a new cycle (next month's statement).
pub const NEW_CYCLE_AFTER_DAYS: i64 = 25;

/// Word overlap at or above which two subjects or titles are "about the
/// same thing".
pub const SIMILAR_AT: f64 = 0.4;

/// A task that a new email might belong to.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Candidate {
    pub task_id: i64,
    pub kind: String,
    pub title: String,
    pub due_date: Option<NaiveDate>,
    /// Account or invoice number, or empty.
    pub reference: String,
    /// Sender addresses of the emails it came from, lowercase.
    pub senders: Vec<String>,
    /// Message-IDs of the emails it came from.
    pub message_ids: Vec<String>,
    /// Subjects of the emails it came from.
    pub subjects: Vec<String>,
    /// Date of the most recent email it came from.
    pub last_mail_date: Option<NaiveDate>,
    /// The amount the task is about, if any.
    pub amount: Option<f64>,
    /// Done or dismissed already.
    pub closed: bool,
}

/// The new email, as decided by extraction.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Incoming {
    pub kind: String,
    pub title: String,
    pub due_date: Option<NaiveDate>,
    pub reference: Option<String>,
    pub sender: String,
    pub subject: String,
    /// Message-IDs this email replies to.
    pub replies_to: Vec<String>,
    pub mail_date: Option<NaiveDate>,
    /// The amount the email names, if any.
    pub amount: Option<f64>,
}

/// Why an email was matched to a task, for the log and the eval report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// A confirmation naming the task's account number.
    PaidReference,
    /// A confirmation for exactly the task's amount.
    PaidAmount,
    /// A confirmation from a sender with exactly one open task.
    PaidOnlyOne,
    /// It replies to an email the task came from.
    Thread,
    /// Same sender, kind, and account number.
    Reference,
    /// Same sender and kind, due dates close together.
    DueDate,
    /// Same sender and kind, no dates to compare, similar subject or
    /// title, soon after.
    Similar,
}

/// The task the email belongs to, if any. Candidates are tried in the
/// order given; give the newest first.
pub fn find_matter(incoming: &Incoming, candidates: &[Candidate]) -> Option<(i64, Reason)> {
    // A reply in the same thread wins regardless of anything else.
    for c in candidates {
        if incoming
            .replies_to
            .iter()
            .any(|id| c.message_ids.contains(id))
        {
            return Some((c.task_id, Reason::Thread));
        }
    }
    for c in candidates {
        if c.kind != incoming.kind || !same_sender(&incoming.sender, &c.senders) {
            continue;
        }
        // Two different account numbers are two different matters.
        let reference = match (&incoming.reference, c.reference.as_str()) {
            (Some(a), b) if !b.is_empty() => {
                if normalize_reference(a) == normalize_reference(b) {
                    Some(true)
                } else {
                    Some(false)
                }
            }
            _ => None,
        };
        if reference == Some(false) {
            continue;
        }
        match (incoming.due_date, c.due_date) {
            (Some(a), Some(b)) => {
                if (a - b).num_days().abs() <= SAME_DUE_WITHIN_DAYS {
                    return Some((
                        c.task_id,
                        if reference == Some(true) {
                            Reason::Reference
                        } else {
                            Reason::DueDate
                        },
                    ));
                }
                // Same sender, same kind, dates far apart: a new cycle.
                continue;
            }
            _ => {
                let gap = match (incoming.mail_date, c.last_mail_date) {
                    (Some(a), Some(b)) => (a - b).num_days().abs(),
                    _ => 0,
                };
                if gap >= NEW_CYCLE_AFTER_DAYS {
                    continue;
                }
                if reference == Some(true) {
                    return Some((c.task_id, Reason::Reference));
                }
                let similar = similarity(&incoming.title, &c.title) >= SIMILAR_AT
                    || c.subjects
                        .iter()
                        .any(|s| similarity(&incoming.subject, s) >= SIMILAR_AT);
                if similar {
                    return Some((c.task_id, Reason::Similar));
                }
            }
        }
    }
    None
}

/// Proof that something got done (a payment confirmation, say): the open
/// task it settles, if the evidence points at exactly one. Strict on
/// purpose: closing the wrong task is worse than leaving one open.
pub fn find_completed(incoming: &Incoming, candidates: &[Candidate]) -> Option<(i64, Reason)> {
    let open: Vec<&Candidate> = candidates.iter().filter(|c| !c.closed).collect();
    // A reply in the thread of the task's own email.
    for c in &open {
        if incoming
            .replies_to
            .iter()
            .any(|id| c.message_ids.contains(id))
        {
            return Some((c.task_id, Reason::Thread));
        }
    }
    let same: Vec<&Candidate> = open
        .iter()
        .copied()
        .filter(|c| same_sender(&incoming.sender, &c.senders))
        .collect();
    if same.is_empty() {
        return None;
    }
    if let Some(r) = &incoming.reference {
        let by_ref: Vec<&Candidate> = same
            .iter()
            .copied()
            .filter(|c| {
                !c.reference.is_empty()
                    && normalize_reference(&c.reference) == normalize_reference(r)
            })
            .collect();
        if by_ref.len() == 1 {
            return Some((by_ref[0].task_id, Reason::PaidReference));
        }
        if by_ref.len() > 1 {
            return None;
        }
    }
    if let Some(a) = incoming.amount {
        let by_amount: Vec<&Candidate> = same
            .iter()
            .copied()
            .filter(|c| c.amount.is_some_and(|b| (a - b).abs() < 0.005))
            .collect();
        if by_amount.len() == 1 {
            return Some((by_amount[0].task_id, Reason::PaidAmount));
        }
        if by_amount.len() > 1 {
            return None;
        }
        // An amount that matches none of the sender's tasks is not proof
        // for any of them.
        if same.iter().any(|c| c.amount.is_some()) {
            return None;
        }
    }
    if same.len() == 1 {
        return Some((same[0].task_id, Reason::PaidOnlyOne));
    }
    None
}

/// Same address, or the same domain unless it is a shared mail provider
/// (two people at gmail are not one sender).
pub fn same_sender(sender: &str, senders: &[String]) -> bool {
    let sender = sender.trim().to_lowercase();
    if sender.is_empty() {
        return false;
    }
    if senders.iter().any(|s| s.trim().to_lowercase() == sender) {
        return true;
    }
    let Some(domain) = sender.rsplit('@').next().filter(|d| d.contains('.')) else {
        return false;
    };
    if is_shared_provider(domain) {
        return false;
    }
    senders.iter().any(|s| {
        s.rsplit('@')
            .next()
            .is_some_and(|d| d.eq_ignore_ascii_case(domain))
    })
}

fn is_shared_provider(domain: &str) -> bool {
    const SHARED: &[&str] = &[
        "gmail.com",
        "googlemail.com",
        "yahoo.com",
        "outlook.com",
        "hotmail.com",
        "live.com",
        "icloud.com",
        "me.com",
        "aol.com",
        "proton.me",
        "protonmail.com",
        "comcast.net",
        "att.net",
    ];
    SHARED.contains(&domain)
}

/// Account numbers compare without spaces, dashes, or case.
pub fn normalize_reference(r: &str) -> String {
    r.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Words that say nothing about which matter an email concerns.
const NOISE: &[&str] = &[
    "re",
    "fw",
    "fwd",
    "reminder",
    "final",
    "second",
    "notice",
    "urgent",
    "friendly",
    "your",
    "the",
    "a",
    "an",
    "is",
    "are",
    "for",
    "to",
    "of",
    "and",
    "on",
    "in",
    "at",
    "by",
    "now",
    "ready",
    "due",
    "please",
    "pay",
    "payment",
    "update",
    "updated",
    "action",
    "required",
    "important",
    "about",
    "regarding",
    "with",
    "from",
    "this",
    "that",
    "it",
    "its",
];

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1 && !NOISE.contains(w))
        .map(str::to_string)
        .collect()
}

/// Jaccard overlap of the meaningful words in two texts, 0 to 1.
pub fn similarity(a: &str, b: &str) -> f64 {
    let (wa, wb) = (words(a), words(b));
    if wa.is_empty() || wb.is_empty() {
        return 0.0;
    }
    let shared = wa.iter().filter(|w| wb.contains(w)).count();
    let union = wa.len() + wb.len() - shared;
    if union == 0 {
        0.0
    } else {
        shared as f64 / union as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Option<NaiveDate> {
        Some(NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap())
    }

    fn electric(task_id: i64) -> Candidate {
        Candidate {
            task_id,
            kind: "bill".into(),
            title: "Pay the City Power bill".into(),
            due_date: d("2026-10-31"),
            reference: "4471-02".into(),
            senders: vec!["billing@citypower.invalid".into()],
            message_ids: vec!["<stmt-oct@citypower.invalid>".into()],
            subjects: vec!["Your October statement is ready".into()],
            last_mail_date: d("2026-10-15"),
            amount: Some(84.12),
            closed: false,
        }
    }

    fn incoming() -> Incoming {
        Incoming {
            kind: "bill".into(),
            title: "Pay the City Power bill".into(),
            due_date: d("2026-10-31"),
            reference: Some("4471-02".into()),
            sender: "billing@citypower.invalid".into(),
            subject: "Reminder: payment due October 31".into(),
            replies_to: vec![],
            mail_date: d("2026-10-25"),
            amount: Some(84.12),
        }
    }

    fn receipt() -> Incoming {
        Incoming {
            kind: "none".into(),
            title: String::new(),
            due_date: None,
            reference: Some("4471-02".into()),
            sender: "billing@citypower.invalid".into(),
            subject: "Thank you for your payment".into(),
            replies_to: vec![],
            mail_date: d("2026-10-28"),
            amount: Some(84.12),
        }
    }

    fn water(task_id: i64) -> Candidate {
        Candidate {
            task_id,
            title: "Pay the Metro Water bill".into(),
            reference: "88-1092".into(),
            senders: vec!["bills@metrowater.invalid".into()],
            message_ids: vec!["<w1@metrowater.invalid>".into()],
            subjects: vec!["Water bill due".into()],
            amount: Some(41.50),
            ..electric(task_id)
        }
    }

    #[test]
    fn a_payment_confirmation_closes_the_matching_task_only() {
        let tasks = [electric(1), water(2)];
        assert_eq!(
            find_completed(&receipt(), &tasks),
            Some((1, Reason::PaidReference))
        );
        // No account number: the amount decides.
        let by_amount = Incoming {
            reference: None,
            ..receipt()
        };
        assert_eq!(
            find_completed(&by_amount, &tasks),
            Some((1, Reason::PaidAmount))
        );
        // Same sender, one open task, nothing else to go on.
        let bare = Incoming {
            reference: None,
            amount: None,
            ..receipt()
        };
        assert_eq!(
            find_completed(&bare, &tasks),
            Some((1, Reason::PaidOnlyOne))
        );
        // A stranger's receipt closes nothing.
        let stranger = Incoming {
            sender: "pay@somewhere.invalid".into(),
            ..receipt()
        };
        assert_eq!(find_completed(&stranger, &tasks), None);
        // An amount that fits none of the sender's bills is not proof.
        let other_amount = Incoming {
            reference: None,
            amount: Some(12.0),
            ..receipt()
        };
        assert_eq!(find_completed(&other_amount, &tasks), None);
        // Two open bills from one sender, no number or amount: ask nobody, close nothing.
        let two = [
            electric(1),
            Candidate {
                task_id: 3,
                reference: "9920-17".into(),
                amount: Some(37.60),
                ..electric(3)
            },
        ];
        assert_eq!(find_completed(&bare, &two), None);
        assert_eq!(
            find_completed(&receipt(), &two),
            Some((1, Reason::PaidReference))
        );
        // Already closed tasks are not candidates.
        let closed = [Candidate {
            closed: true,
            ..electric(1)
        }];
        assert_eq!(find_completed(&receipt(), &closed), None);
    }

    #[test]
    fn a_reminder_for_the_same_bill_attaches() {
        let got = find_matter(&incoming(), &[electric(1)]);
        assert_eq!(got, Some((1, Reason::Reference)));
        let no_ref = Incoming {
            reference: None,
            ..incoming()
        };
        assert_eq!(
            find_matter(&no_ref, &[electric(1)]),
            Some((1, Reason::DueDate))
        );
    }

    #[test]
    fn a_final_notice_with_a_pushed_due_date_still_attaches() {
        let pushed = Incoming {
            due_date: d("2026-11-14"),
            subject: "FINAL NOTICE".into(),
            mail_date: d("2026-11-03"),
            ..incoming()
        };
        assert_eq!(
            find_matter(&pushed, &[electric(1)]),
            Some((1, Reason::Reference))
        );
    }

    #[test]
    fn next_months_bill_is_a_new_matter() {
        let november = Incoming {
            due_date: d("2026-11-30"),
            subject: "Your November statement is ready".into(),
            mail_date: d("2026-11-15"),
            ..incoming()
        };
        assert_eq!(find_matter(&november, &[electric(1)]), None);
    }

    #[test]
    fn a_different_account_at_the_same_sender_is_a_new_matter() {
        let gas = Incoming {
            reference: Some("9920-17".into()),
            title: "Pay the City Power gas bill".into(),
            ..incoming()
        };
        assert_eq!(find_matter(&gas, &[electric(1)]), None);
    }

    #[test]
    fn a_reply_in_the_thread_attaches_whatever_else_differs() {
        let reply = Incoming {
            kind: "reply_needed".into(),
            title: "Answer City Power about the meter".into(),
            due_date: None,
            reference: None,
            sender: "someone-else@citypower.invalid".into(),
            replies_to: vec!["<stmt-oct@citypower.invalid>".into()],
            ..incoming()
        };
        assert_eq!(
            find_matter(&reply, &[electric(1)]),
            Some((1, Reason::Thread))
        );
    }

    #[test]
    fn without_dates_similar_subjects_soon_after_attach_but_not_a_month_later() {
        let undated_task = Candidate {
            due_date: None,
            reference: String::new(),
            ..electric(2)
        };
        let nudge = Incoming {
            due_date: None,
            reference: None,
            subject: "Reminder: your October statement".into(),
            mail_date: d("2026-10-22"),
            ..incoming()
        };
        assert_eq!(
            find_matter(&nudge, std::slice::from_ref(&undated_task)),
            Some((2, Reason::Similar))
        );
        let later = Incoming {
            mail_date: d("2026-11-15"),
            ..nudge
        };
        assert_eq!(find_matter(&later, &[undated_task]), None);
    }

    #[test]
    fn senders_match_by_address_or_company_domain_only() {
        let company = vec!["billing@citypower.invalid".to_string()];
        assert!(same_sender("Billing@CityPower.invalid", &company));
        assert!(same_sender("notices@citypower.invalid", &company));
        assert!(!same_sender("billing@other.invalid", &company));
        let person = vec!["pat@gmail.com".to_string()];
        assert!(same_sender("pat@gmail.com", &person));
        assert!(!same_sender("sam@gmail.com", &person));
    }

    #[test]
    fn similarity_ignores_noise_words() {
        assert!(
            similarity(
                "Your October statement is ready",
                "Reminder: October statement"
            ) > 0.6
        );
        assert!(similarity("Pay the City Power bill", "Renew the car registration") < 0.2);
        assert_eq!(
            normalize_reference("4471-02"),
            normalize_reference("4471 02")
        );
    }
}
