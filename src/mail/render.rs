//! Mail presentation over owned observations and explicit durable receipts.
use super::operations::*;
use crate::out::Out;
use crate::storage::Noted;
use anyhow::Result;

pub fn account_set(value: &AccountSetReceipt, out: &mut Out<'_>) -> Result<()> {
    if value.changed {
        out.line(format!(
            "Account {:x} config {:x}",
            value.account, value.config
        ))
    } else {
        out.line(format!(
            "Account {:x} already has config {:x}",
            value.account, value.config
        ))
    }
}
pub fn accounts(read: &Noted<Vec<AccountSummary>>, out: &mut Out<'_>) -> Result<()> {
    out.notes(&read.notes)?;
    let values = &read.value;
    for value in values {
        match &value.state {
            AccountState::Missing => out.line(format!("{:x}  MISSING", value.account))?,
            AccountState::Forked(ids) => {
                out.line(format!("{:x}  FORKED {ids:?}", value.account))?
            }
            AccountState::Configured {
                config,
                address,
                enabled,
                quiet,
            } => out.line(format!(
                "{:x}  {}  {}{}  config={:x}",
                value.account,
                address,
                if *enabled { "enabled" } else { "disabled" },
                if *quiet { "  quiet" } else { "" },
                config
            ))?,
        }
    }
    Ok(())
}
pub fn fetched(values: &[AccountFetched], out: &mut Out<'_>) -> Result<()> {
    for value in values {
        out.line(format!(
            "{}: fetched {} message(s)",
            value.address, value.fetched
        ))?;
        for mailbox in &value.mailboxes {
            out.line(format!(
                "  {:?} uidvalidity={}: {} matched, {} already held, {} fetched{}",
                mailbox.mailbox,
                mailbox.uidvalidity,
                mailbox.matched,
                mailbox.held,
                mailbox.fetched,
                if mailbox.vanished > 0 {
                    format!(", {} vanished", mailbox.vanished)
                } else {
                    String::new()
                }
            ))?;
            for (uid, size) in &mailbox.oversized {
                out.line(format!(
                    "    UID {uid}: {size} bytes, over the {}-byte cap; left unread on the server",
                    crate::mail::MAX_MESSAGE_BYTES
                ))?;
            }
            for (uid, reason) in &mailbox.unparsed {
                out.line(format!("    UID {uid}: not recorded: {reason}"))?;
            }
        }
    }
    Ok(())
}
pub fn draft(value: &DraftReceipt, out: &mut Out<'_>) -> Result<()> {
    out.line(format!("Draft {:x}", value.draft))?;
    for (label, recipients) in [("To", &value.to), ("Cc", &value.cc), ("Bcc", &value.bcc)] {
        if !recipients.is_empty() {
            out.line(format!("{label}: {}", recipients.join(", ")))?;
        }
    }
    out.line(format!("Decision {:x}", value.decision))
}
pub fn sent(value: &SendReceipt, out: &mut Out<'_>) -> Result<()> {
    match &value.status {
        SendStatus::AlreadyAccepted => out.line(format!(
            "Draft {:x} was already accepted (attempt {:x}).",
            value.draft, value.attempt
        )),
        SendStatus::Accepted(response) => out.line(format!(
            "Accepted draft {:x} as attempt {:x}: {} {}",
            value.draft, value.attempt, response.code, response.message
        )),
    }
}
pub fn outbox(read: &Noted<Vec<DraftStatus>>, out: &mut Out<'_>) -> Result<()> {
    out.notes(&read.notes)?;
    let values = &read.value;
    for value in values {
        let state = match value.delivery {
            DraftDelivery::Pending => "pending".to_owned(),
            DraftDelivery::Uncertain(id) => format!("UNCERTAIN attempt={id:x}"),
            DraftDelivery::Accepted(id) => format!("accepted attempt={id:x}"),
            DraftDelivery::MultipleAttempts => "INVALID multiple attempts".to_owned(),
        };
        out.line(format!("{:x}  {}  {}", value.draft, state, value.subject))?;
    }
    Ok(())
}
pub fn inbox(read: &Noted<Vec<InboxMessage>>, out: &mut Out<'_>) -> Result<()> {
    out.notes(&read.notes)?;
    let values = &read.value;
    for value in values {
        let view = &value.projection;
        out.line(format!(
            "{:x}  {}{}  {}  {}",
            view.wire,
            if value.unread { "UNREAD" } else { "read" },
            if view.spam { "/spam" } else { "" },
            view.from.as_deref().unwrap_or("(no From)"),
            view.subject
        ))?;
    }
    Ok(())
}
pub fn read(value: &Noted<ReadReceipt>, out: &mut Out<'_>) -> Result<()> {
    out.notes(&value.notes)?;
    out.line(format!("Read {:x} ({:x})", value.wire, value.observation))
}
pub fn show(read: &Noted<Vec<super::ProjectionView>>, out: &mut Out<'_>) -> Result<()> {
    out.notes(&read.notes)?;
    let values = &read.value;
    for view in values {
        out.line(format!("Wire: {:x}", view.wire))?;
        out.line(format!(
            "Message-ID: {}",
            view.message_id.as_deref().unwrap_or("(not claimed)")
        ))?;
        out.line(format!("Source: {:x}", view.source))?;
        out.line(format!(
            "From: {}",
            view.from.as_deref().unwrap_or_default()
        ))?;
        out.line(format!("To: {}", view.to.join(", ")))?;
        if !view.cc.is_empty() {
            out.line(format!("Cc: {}", view.cc.join(", ")))?;
        }
        out.line(format!("Subject: {}", view.subject))?;
        out.line("")?;
        out.line(&view.body)?;
        if !view.attachments.is_empty() {
            out.line(format!(
                "\nAttachment occurrences: {}",
                view.attachments
                    .iter()
                    .map(|id| format!("{id:x}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))?;
        }
    }
    Ok(())
}
pub fn search(read: &Noted<Vec<super::ProjectionView>>, out: &mut Out<'_>) -> Result<()> {
    out.notes(&read.notes)?;
    let values = &read.value;
    for view in values {
        out.line(format!(
            "{:x}  {}  {}",
            view.wire,
            view.from.as_deref().unwrap_or_default(),
            view.subject
        ))?;
    }
    Ok(())
}
pub fn attachments(
    read: &Noted<Vec<AttachmentListing>>,
    saved: Option<&[super::cli::Saved]>,
    out: &mut Out<'_>,
) -> Result<()> {
    out.notes(&read.notes)?;
    let values = &read.value;
    if values.is_empty() {
        return out.line("No attachments.");
    }
    for (index, value) in values.iter().enumerate() {
        let ordinal = value
            .ordinal
            .map_or_else(|| "?".to_owned(), |ordinal| ordinal.to_string());
        let mut line = match &value.state {
            AttachmentState::Resident {
                filename,
                media_type,
                content,
                bytes,
            } => format!(
                "{ordinal}  {filename:?}  {media_type}  {} bytes  blake3:{}  occurrence {:x}",
                bytes.len(),
                hex::encode(content),
                value.occurrence
            ),
            AttachmentState::NotResident(what) => format!(
                "{ordinal}  not resident ({what})  occurrence {:x}",
                value.occurrence
            ),
        };
        match saved.and_then(|saved| saved.get(index)) {
            Some(super::cli::Saved::Written(path)) => {
                line.push_str(&format!("  saved {}", path.display()))
            }
            Some(super::cli::Saved::Present(path)) => {
                line.push_str(&format!("  already saved {}", path.display()))
            }
            Some(super::cli::Saved::Refused(reason)) => {
                line.push_str(&format!("  NOT SAVED: {reason}"))
            }
            Some(super::cli::Saved::Skipped) | None => {}
        }
        out.line(line)?;
    }
    Ok(())
}
