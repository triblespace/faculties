//! Care schema: one immutable observation per health reading.
//!
//! An observation is a private relation. Its facts never appear in a
//! collection as plaintext: the writer seals the observation's archive, with
//! its note and photo inside, as one Secrets record (`secrets::record`), and
//! only [`KIND_OBSERVATION_RECORD`] on that version's id is public. Kinds, units and methods are plain entities named
//! by `metadata::name`; LOINC and UCUM codes are annotations, never identity.
//!
//! Every id below is an exact output of installed `trible genid`, 2026-10-31.

use crate::secrets::encrypted::Encrypted;
use triblespace::macros::id_hex;
use triblespace::prelude::blobencodings::{RawBytes, UTF8String};
use triblespace::prelude::inlineencodings::{GenId, Handle, NsTAIInterval, F64};
use triblespace::prelude::*;

/// Tag of one observation entity inside a sealed record.
pub const KIND_OBSERVATION: Id = id_hex!("88799AE91CED5F960D0E3D6B4026C76E");
/// Public tag on a Secrets version whose plaintext is one observation archive.
pub const KIND_OBSERVATION_RECORD: Id = id_hex!("48EBBA4464EA22DF79B9B1A955CCAF02");
/// Tag of an observation kind entity (what was observed).
pub const KIND_OBSERVATION_KIND: Id = id_hex!("B9E1E11F638675608DD5115EFBD8FD70");
/// Tag of a unit entity.
pub const KIND_UNIT: Id = id_hex!("9FFE1A27345E7CDD5C1889A32552BD79");
/// Tag of an observation method entity.
pub const KIND_METHOD: Id = id_hex!("436181C706F2879F76E308C4ECEFDF44");

pub mod care {
    use super::*;
    attributes! {
        /// The person observed: a Relations person anchor.
        "5431A725224338195E7F9AAC5671691B" as pub subject: GenId;
        /// What was observed: a kind entity.
        "C94942EC59944B157C6C7E67859C751E" as pub kind: GenId;
        /// The observed (or estimated) value, in `unit`.
        "58D8EC9581B2EC455D12CCF7A3BC011A" as pub value: F64;
        /// Lower bound of an estimate, in `unit`.
        "3D7ECF504164DBD522FC5D87C2308C4A" as pub low: F64;
        /// Upper bound of an estimate, in `unit`.
        "ECF3129A6358B9CA8B51CBAE54CF7618" as pub high: F64;
        /// Unit of value and bounds: a unit entity.
        "8E1B4DE4763587502733DC2D34F35CBE" as pub unit: GenId;
        /// When the reading happened; a wide interval when only approximate.
        "6C220A0F7D961FDEC064EA373E3B107D" as pub observed_at: NsTAIInterval;
        /// When the reading was reported to the recorder.
        "CF41DFB364AC4EE5A538ABC26E088561" as pub reported_at: NsTAIInterval;
        /// How the value was obtained: a method entity.
        "66EF94825F2E9E05A51495EB99EB65AA" as pub method: GenId;
        /// Source photo bytes, sealed inside the same record.
        "B21416CCC43847616AB9CE57E593DCAB" as pub photo: Handle<Encrypted<RawBytes>>;
        /// Source Discord message: its stable message anchor.
        "09779FC092A999368645FF34936C50CD" as pub message: GenId;
        /// Free-text note, sealed inside the same record.
        "3733C93EF26C524D152071260B6B0170" as pub note: Handle<Encrypted<UTF8String>>;
        /// LOINC code annotation on a kind entity.
        "F1A8157B2E3D18629828629E02FFE260" as pub loinc: Handle<UTF8String>;
        /// UCUM code annotation on a unit entity.
        "AB07CABC724C9C2EE571BE4CE6CD6714" as pub ucum: Handle<UTF8String>;
    }
}

pub const KIND_SYSTOLIC_BP: Id = id_hex!("FA5661455089FE0DB961186AD21269EE");
pub const KIND_DIASTOLIC_BP: Id = id_hex!("1B30F17CFF4A743EA93A2BECB123B237");
pub const KIND_HEART_RATE: Id = id_hex!("2F237E17A435760A0AE8ABA9D8DAD7D6");
pub const KIND_BODY_TEMPERATURE: Id = id_hex!("84520BAC3971FB7B93DF0D19E616DE82");
pub const KIND_URINE_OUTPUT: Id = id_hex!("FFE8A95A3CDA33FBEBF2F790AAC1A3CE");
pub const KIND_FLUID_INTAKE: Id = id_hex!("5B9BEB6D84C8F2329B422191C074669A");
pub const KIND_PAIN_SEVERITY: Id = id_hex!("23AC5ADA7FEAF4459682447D61D2F1C1");

pub const UNIT_MM_HG: Id = id_hex!("5F5D986D775C17E5BDDAD517D406DC0B");
pub const UNIT_PER_MINUTE: Id = id_hex!("3B6BF644F10D7765AE41AEDE5A486F65");
pub const UNIT_CELSIUS: Id = id_hex!("52CA05D2276C88C13E3508B1E24A95E0");
pub const UNIT_MILLILITER: Id = id_hex!("06A485E4F7D346371CB69257B959225C");
pub const UNIT_SCORE: Id = id_hex!("8192E6D173D9233F316E003783EE3771");

pub const METHOD_MEASURED_DEVICE: Id = id_hex!("1C5AEDBBE49ECA9D4E52BA0E9147472B");
pub const METHOD_PHOTO_ESTIMATE: Id = id_hex!("1E3035CBB297A1795F61623B1FB8C3B5");
pub const METHOD_REPORTED_BY_SUBJECT: Id = id_hex!("3876580E52E44B6850BAF7116AC680EA");
pub const METHOD_ADMINISTERED: Id = id_hex!("17A3E4027941A71269D2C963405C9450");
