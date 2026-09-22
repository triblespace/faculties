//! Trigger owns check definitions, executions, and results. Historical Habit
//! and Posture facts keep their original vocabulary in this same collection.
//!
//! All anchors below were minted with `trible genid` on 2026-09-22. Attributes
//! use anchored identity (including their encoding), not literal-id pinning.

use triblespace::prelude::*;

pub const DEFAULT_SCOPE_ID: Id = id_hex!("4634FE1E28E12954771242E7590F298A");
pub const KIND_TRIGGER: Id = id_hex!("910F73ED9745D6D3DB48A643BB760080");
pub const KIND_RUN: Id = id_hex!("32945028F489D09476A586D914B958FD");
pub const KIND_RESULT: Id = id_hex!("92884426648F1D3FA2B35172BAD516C1");
pub const KIND_STATE: Id = id_hex!("A3DC41F4B1551761D1DE66A947D03844");

pub mod attrs {
    use super::*;
    // These meanings are unchanged. Sharing the attributes does not make an
    // old Habit definition executable under the new check contract.
    pub use crate::schemas::habit::attrs::{label, persona, script, state};

    attributes! {
        /// Positive fixed timer period in seconds; not an acknowledgement cooldown.
        "FE962F0CC5C28D77E2CAA7D4B73B94A7" as interval_seconds: inlineencodings::U256BE;
        /// Literal shell check, run with explicit cwd, input and execution context.
        "C3BBAD8ADE4045EF134F8FE0A7A46C7B" as command: inlineencodings::Handle<blobencodings::UTF8String>;
        /// timer, advisory-event or synchronous-event. Never inferred from exit code.
        "CA51B07990463513470D1B468762DB69" as context: inlineencodings::ShortString;
        /// Definition observed by a run or a pause/resume assertion.
        "C005FCEE8712FB73925E6ADACFF93293" as trigger_of: inlineencodings::GenId;
        /// Run whose output or verdict this evidence describes.
        "4A494DEA630C4CE7A168DD5147ECA79D" as run_of: inlineencodings::GenId;
        /// Timer origin on a definition; scheduled slot on an execution.
        "71791D1921B49D3CB529E3D9089ED3E8" as scheduled_at: inlineencodings::NsTAIInterval;
        /// Byte-exact standard output, including the empty stream.
        "B302EEA0774C79C7FE113C3E7BC93D05" as stdout: inlineencodings::Handle<blobencodings::RawBytes>;
        /// Byte-exact standard error; evidence, not automatically a notification.
        "E94563FE538B5DE4DBCB79553F208F0F" as stderr: inlineencodings::Handle<blobencodings::RawBytes>;
        /// Execution status, distinct from the policy verdict a successful check returns.
        "7514929E24F3ADCF26FB798CD54A000B" as status: inlineencodings::ShortString;
        /// Execution diagnostic (exit code, signal, or error), not check stdout.
        "D2E622B35950AEC5F7BD7838006E90FF" as detail: inlineencodings::Handle<blobencodings::UTF8String>;
        /// Durable host identity executing the check. Not a second network key.
        "7624B38BDFEDD0F86C12EA91A22D43E5" as executor: inlineencodings::ED25519PublicKey;
        /// Event name at the caller boundary, e.g. pre-push or post-commit.
        "FEEDDC816175B0CAF6E9E7AB1C45DE95" as event: inlineencodings::ShortString;
        /// Synchronous caller verdict. A successful policy check may still deny.
        "18D9BCAC27A304DE053EF9D9B07A9A8E" as verdict: inlineencodings::I256BE;
        /// Original definition from which this explicitly converted check was made.
        "560D6E93151D568B4118F60BB60F97D8" as source_definition: inlineencodings::GenId;
        /// Opaque occurrence supplied by a repository/event observer. Joining
        /// this evidence handles redelivery without reconstructing its ID.
        "9F8D65AB68C59B7E2E7F63E39CE65BA3" as occurrence: inlineencodings::GenId;
    }
}
