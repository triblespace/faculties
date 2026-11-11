//! Attribute identity regression checks.
//!
//! A quoted id followed by bare `as` is an *anchor* in the current
//! `attributes!` macro: the encoding participates in the resulting id. The
//! faculties predate that distinction, so their published literals must use
//! the explicitly pinned `unsafe as` form to keep addressing existing facts.

fn assert_id(path: &str, got: String, expected: &str) {
    assert_eq!(
        got, expected,
        "{path} no longer resolves to its published id"
    );
}

#[test]
fn durable_faculty_attributes_keep_their_published_ids() {
    assert_id(
        "wiki::fragment",
        format!("{:X}", crate::schemas::wiki::attrs::fragment.id()),
        "EBFC56D50B748E38A14F5FC768F1B9C1",
    );
    assert_id(
        "files::content",
        format!("{:X}", crate::schemas::files::file::content.id()),
        "C1E3A12230595280F22ABEB8733D082C",
    );
    assert_id(
        "relations::group::member",
        format!("{:X}", crate::schemas::relations::group::member.id()),
        "EF5B6F8429FA30D503BA8B8F3ABD5FD9",
    );
    assert_id(
        "message::to",
        format!("{:X}", crate::schemas::message::local::to.id()),
        "95D58D3E68A43979F8AA51415541414C",
    );
    assert_id(
        "compass::title",
        format!("{:X}", crate::schemas::compass::board::title.id()),
        "EE18CEC15C18438A2FAB670E2E46E00C",
    );
    assert_id(
        "mail::from",
        format!("{:X}", crate::schemas::mail::mail::from.id()),
        "CFAEF6367467548E6799AA8AE9E971C8",
    );
    assert_id(
        "planner::cancellation::event",
        format!("{:X}", crate::schemas::planner::cancellation::event.id()),
        "123D7A7CC84D0E95EE51298021213B46",
    );
    assert_id(
        "triage::exec::attempt",
        format!("{:X}", crate::schemas::triage::exec::attempt.id()),
        "79474B948670C7D0322C309EB65219F8",
    );
    assert_id(
        "triage::model_chat::attempt",
        format!("{:X}", crate::schemas::triage::model_chat::attempt.id()),
        "8CAEF4617646F8C9E90BC9A3ED3D0496",
    );
    for (path, got, expected) in [
        // Anchored text since the ShortString retirement: derived from
        // anchor 4AB58053472C4163C39C9A4047F8111E and Handle<UTF8String>.
        (
            "habit::label",
            format!("{:X}", crate::schemas::habit::attrs::label.id()),
            "5F6655CE0010D8271647F34319FDB318",
        ),
        (
            "habit::nudge",
            format!("{:X}", crate::schemas::habit::attrs::nudge.id()),
            "1E82BD7E8EFEA00FD3FAB3ECEFD0BA33",
        ),
        (
            "habit::condition",
            format!("{:X}", crate::schemas::habit::attrs::condition.id()),
            "134ECC925E8547B46AF67D6DC29B5F5C",
        ),
        (
            "habit::of",
            format!("{:X}", crate::schemas::habit::attrs::of.id()),
            "F00FAA4B44DB1E79E36055410B476C42",
        ),
        // Anchored text since the ShortString retirement: derived from
        // anchor 5C1E4BD13E8FA4633F286CD5B33BCAC7 and Handle<UTF8String>.
        (
            "habit::state",
            format!("{:X}", crate::schemas::habit::attrs::state.id()),
            "66049DE2A50C6B0BAF3B1B5EF29470A2",
        ),
        // Anchored, not pinned: the recorded id is the value derived from
        // anchor 96EC24A8226E9D848A4905D982485678 and the
        // Handle<RawBytes> encoding, so changing either is caught here.
        (
            "habit::script",
            format!("{:X}", crate::schemas::habit::attrs::script.id()),
            "9B053A1DFE1A091635E8D619B03B9FB1",
        ),
    ] {
        assert_id(path, got, expected);
    }
}

#[test]
fn relations_snapshot_algebra_keeps_its_minted_ids() {
    use crate::schemas::relations::{group, identity, lifecycle, profile};

    for (path, got, expected) in [
        (
            "relations::profile::of",
            format!("{:X}", profile::of.id()),
            "6BB0306AA13B62F7E5490AEB255430E3",
        ),
        (
            "relations::profile::alias",
            format!("{:X}", profile::alias.id()),
            "8663728605F1212E3B454D0E7F09FB76",
        ),
        (
            "relations::profile::affinity",
            format!("{:X}", profile::affinity.id()),
            "96101F2E1A20978BEBD12BB97D6E84F6",
        ),
        (
            "relations::profile::teams_user_id",
            format!("{:X}", profile::teams_user_id.id()),
            "9DBA8FAEF649E33919BC708F943F0C2D",
        ),
        (
            "relations::profile::email",
            format!("{:X}", profile::email.id()),
            "962F91429CE0432204B12E9A041E56A8",
        ),
        (
            "relations::profile::phone",
            format!("{:X}", profile::phone.id()),
            "140A6AAD3F1845694F33B00D97B9AF40",
        ),
        (
            "relations::profile::first_name",
            format!("{:X}", profile::first_name.id()),
            "F0AD0BBFAC4C4C899637573DC965622E",
        ),
        (
            "relations::profile::last_name",
            format!("{:X}", profile::last_name.id()),
            "764DD765142B3F4725B614BD3B9118EC",
        ),
        (
            "relations::profile::display_name",
            format!("{:X}", profile::display_name.id()),
            "DC0916CB5F640984EFE359A33105CA9A",
        ),
        (
            "relations::profile::company",
            format!("{:X}", profile::company.id()),
            "E3D486BD7C9C088D908DF1B9E1F4D925",
        ),
        (
            "relations::profile::position",
            format!("{:X}", profile::position.id()),
            "173B771D35FEE90B83F2731DD3C59EF8",
        ),
        (
            "relations::profile::profile_url",
            format!("{:X}", profile::profile_url.id()),
            "5A71C103E026FC1AC01E35EDAC274A5C",
        ),
        (
            "relations::lifecycle::of",
            format!("{:X}", lifecycle::of.id()),
            "36E4966DA6704AA84C44A3E4E8DEB70F",
        ),
        (
            "relations::lifecycle::retired",
            format!("{:X}", lifecycle::retired.id()),
            "639BD621C86B6B6C39F08D6E97026988",
        ),
        (
            "relations::group::member",
            format!("{:X}", group::member.id()),
            "EF5B6F8429FA30D503BA8B8F3ABD5FD9",
        ),
        (
            "relations::group::snapshot_of",
            format!("{:X}", group::snapshot_of.id()),
            "D944552B560826095BCEAFDAACE6DF66",
        ),
        (
            "relations::identity::low",
            format!("{:X}", identity::low.id()),
            "31B34A0C3B2129DA19ECEF84961E92EC",
        ),
        (
            "relations::identity::high",
            format!("{:X}", identity::high.id()),
            "86B8EF9DA613C443C27A1A9519222CBE",
        ),
        (
            "relations::identity::same",
            format!("{:X}", identity::same.id()),
            "EFBE40002918177DCBAAEC2D20D223FD",
        ),
    ] {
        assert_id(path, got, expected);
    }

    for (path, got, expected) in [
        (
            "relations::DEFAULT_SCOPE_ID",
            format!("{:X}", crate::schemas::relations::DEFAULT_SCOPE_ID),
            "A36AB53B3F9B4D52AC6BD473C1F8C4F1",
        ),
        (
            "relations::KIND_PERSON_PROFILE",
            format!("{:X}", crate::schemas::relations::KIND_PERSON_PROFILE),
            "BEFF639D71F2AF70BC01E0DBE99C0304",
        ),
        (
            "relations::KIND_PERSON_LIFECYCLE",
            format!("{:X}", crate::schemas::relations::KIND_PERSON_LIFECYCLE),
            "717DCED8539A871037AFFC7893F6FF9F",
        ),
        (
            "relations::KIND_GROUP_SNAPSHOT",
            format!("{:X}", crate::schemas::relations::KIND_GROUP_SNAPSHOT),
            "A42E379E89D2F3A52EEA7A40771B51BF",
        ),
        (
            "relations::KIND_IDENTITY_VERDICT",
            format!("{:X}", crate::schemas::relations::KIND_IDENTITY_VERDICT),
            "4BEAD16C2FDBBDEB7BA37B464594E1CE",
        ),
    ] {
        assert_id(path, got, expected);
    }
}

/// The Alarm schema's ids, minted with `trible genid` on 2026-11-11. The
/// constants are pinned byte-exact. The attributes are anchored, so each
/// recorded id is the value derived from its minted anchor (in
/// `schemas::alarm`) and its encoding, and a change of either is caught here.
#[test]
fn alarm_ids_keep_their_minted_values() {
    use crate::schemas::alarm::{self as schema, alarm};

    let mut drifted = Vec::new();
    for (path, got, expected) in [
        (
            "alarm::of",
            format!("{:X}", alarm::of.id()),
            "92D7B7AABD7EF269D4EFCC1B3F2F22BA",
        ),
        (
            "alarm::decays",
            format!("{:X}", alarm::decays.id()),
            "F647D5B1B14FCA07814EB1EACD7B63C1",
        ),
        (
            "alarm::evidence",
            format!("{:X}", alarm::evidence.id()),
            "F80535FDE2B1DA331095AD4824E178E8",
        ),
        (
            "alarm::incident",
            format!("{:X}", alarm::incident.id()),
            "CCD194F72F49F62148DC5095A397AE81",
        ),
        (
            "alarm::rung",
            format!("{:X}", alarm::rung.id()),
            "2552758B96077A310B8A63533251DCC4",
        ),
        (
            "alarm::via",
            format!("{:X}", alarm::via.id()),
            "260AC3E516461D2ADF6C5A608B4A9234",
        ),
        (
            "alarm::for_person",
            format!("{:X}", alarm::for_person.id()),
            "9DD0263DEF9000161152C5DB890341D1",
        ),
    ] {
        if got != expected {
            drifted.push(format!("{path}: {got} (pinned {expected})"));
        }
    }

    for (path, got, expected) in [
        (
            "DEFAULT_SCOPE_ID",
            schema::DEFAULT_SCOPE_ID,
            "16F26F0830541F6B8C8C080BB66758A4",
        ),
        (
            "KIND_ALARM",
            schema::KIND_ALARM,
            "9E1AA740F7555FD47295C0F10E50753E",
        ),
        (
            "KIND_ACK",
            schema::KIND_ACK,
            "4E2A034FB34F5962FBAEBB2516A2434D",
        ),
        (
            "KIND_RESOLVE",
            schema::KIND_RESOLVE,
            "BA6E1060E775C1FB982D2C5C668F7A6C",
        ),
        (
            "KIND_NOTE",
            schema::KIND_NOTE,
            "D3CC6BF340AE35337D532369710D4C0D",
        ),
        (
            "KIND_STEP",
            schema::KIND_STEP,
            "83B784CC26C756591772C4BDE5C59F28",
        ),
        (
            "KIND_STEP_DELIVERED",
            schema::KIND_STEP_DELIVERED,
            "30CC8AC8BBC08D28FDA4DF152E824DFE",
        ),
        (
            "KIND_STEP_FAILED",
            schema::KIND_STEP_FAILED,
            "438B08CE4F8B32C82872D186E76874B1",
        ),
        (
            "KIND_TEST",
            schema::KIND_TEST,
            "B481E6D1AF08C281354359E9859BF72C",
        ),
        (
            "KIND_FALSE_ALARM",
            schema::KIND_FALSE_ALARM,
            "622345A4D7BCD5584DE7E2C8D7AB071E",
        ),
        (
            "CLASS_DATA_LOSS",
            schema::CLASS_DATA_LOSS,
            "F30BAA9AE12E21BDC9C0A49B4DC890F5",
        ),
        (
            "CLASS_RECOVERY_WINDOW",
            schema::CLASS_RECOVERY_WINDOW,
            "2FEAC5443995B3CF5BEA937C30B8C99E",
        ),
        (
            "CLASS_HUNG_HOST",
            schema::CLASS_HUNG_HOST,
            "4BAC7221E2B42150B7DD9DCB7E5C9ECB",
        ),
        (
            "CLASS_SECURITY",
            schema::CLASS_SECURITY,
            "19902F164719EF63D47CA331879E133A",
        ),
        (
            "RUNG_COLONY",
            schema::RUNG_COLONY,
            "F4E6FCC62A5C14921D44DBD2A508C545",
        ),
        (
            "RUNG_DISCORD_MENTION",
            schema::RUNG_DISCORD_MENTION,
            "94AE05554D4C2470A9625D0F7FC58112",
        ),
        (
            "RUNG_DISCORD_VOICE",
            schema::RUNG_DISCORD_VOICE,
            "2445FE1A93253B871E512173C73AAEBD",
        ),
        (
            "RUNG_MAC_NOTIFY",
            schema::RUNG_MAC_NOTIFY,
            "CA5596B34C32DA4DEE63B076FF8D6E27",
        ),
        (
            "RUNG_MAC_SOUND",
            schema::RUNG_MAC_SOUND,
            "22998FE7FFB6159EEB87E03ABAC5D3F5",
        ),
        (
            "RUNG_REACHY_SHOUT",
            schema::RUNG_REACHY_SHOUT,
            "A814C945124580BFEC2315E1B8143B86",
        ),
        (
            "RUNG_HUE_ALERT",
            schema::RUNG_HUE_ALERT,
            "F867BB343CE049F3BEA072687589CEA9",
        ),
        (
            "RUNG_FINAL_DISCORD",
            schema::RUNG_FINAL_DISCORD,
            "3BE15BB9D7E0C8954050B924399E12FF",
        ),
        (
            "RUNG_FINAL_COLONY",
            schema::RUNG_FINAL_COLONY,
            "58D8A3B2EB98BE2A24BD7EB17A4DFCB5",
        ),
        (
            "VIA_CLI",
            schema::VIA_CLI,
            "0ACF79419247D63ED199602D7407838F",
        ),
        (
            "VIA_RELAYED",
            schema::VIA_RELAYED,
            "E9F88F3150934517FFC4FFC207C229F4",
        ),
        (
            "VIA_DISCORD_REPLY",
            schema::VIA_DISCORD_REPLY,
            "0B6EC3EACCAB8B57E3316F4F6473F5CF",
        ),
        (
            "VIA_DISCORD_LINE",
            schema::VIA_DISCORD_LINE,
            "006D7F135067837D9D10BCB54303BE22",
        ),
        (
            "VIA_VOICE",
            schema::VIA_VOICE,
            "15EA7DFF6E70293D2BC151833518222E",
        ),
    ] {
        let got = format!("{got:X}");
        if got != expected {
            drifted.push(format!("{path}: {got} (pinned {expected})"));
        }
    }
    assert!(
        drifted.is_empty(),
        "alarm ids no longer resolve to their published values:\n{}",
        drifted.join("\n")
    );
}
