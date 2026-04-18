//! Orient schema: checkpoint state for the orient faculty plus the subset
//! of message, compass, and config attributes it reads.
//!
//! Used by `orient.rs` (the faculty CLI). The checkpoint attributes are
//! unique to this faculty; the shared message/board/config attributes are
//! duplicated here so orient can stay self-contained.

use triblespace::macros::id_hex;
use triblespace::prelude::*;

pub const CONFIG_BRANCH_ID: Id = id_hex!("6069A136254E1B87E4C0D2E0295DB382");

pub const KIND_MESSAGE_ID: Id = id_hex!("A3556A66B00276797FCE8A2742AB850F");
pub const KIND_READ_ID: Id = id_hex!("B663C15BB6F2BF591EA870386DD48537");
pub const KIND_GOAL_ID: Id = id_hex!("83476541420F46402A6A9911F46FBA3B");
pub const KIND_STATUS_ID: Id = id_hex!("89602B3277495F4E214D4A417C8CF260");
pub const KIND_ORIENT_CHECKPOINT_ID: Id = id_hex!("163114E5F2272D15F21E1994EF418A31");

pub const CONFIG_KIND_ID: Id = id_hex!("A8DCBFD625F386AA7CDFD62A81183E82");

pub mod local {
    use super::*;
    attributes! {
        "42C4DB210F7EAFAF38F179ADCB4A9D5B" as from: valueschemas::GenId;
        "95D58D3E68A43979F8AA51415541414C" as to: valueschemas::GenId;
        "23075866B369B5F393D43B30649469F6" as body: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;

        "2213B191326E9B99605FA094E516E50E" as about_message: valueschemas::GenId;
        "99E92F483731FA6D59115A8D6D187A37" as reader: valueschemas::GenId;
        "CFEF2E96BC66FF3BE0A39C34E70A5032" as read_at: valueschemas::NsTAIInterval;
    }
}

pub mod config_schema {
    use super::*;

    attributes! {
        "79F990573A9DCC91EF08A5F8CBA7AA25" as kind: valueschemas::GenId;
        "D1DC11B303725409AB8A30C6B59DB2D7" as persona_id: valueschemas::GenId;
    }
}

pub mod board {
    use super::*;
    attributes! {
        "EE18CEC15C18438A2FAB670E2E46E00C" as title: valueschemas::Handle<valueschemas::Blake3, blobschemas::LongString>;
        "5FF4941DCC3F6C35E9B3FD57216F69ED" as tag: valueschemas::ShortString;
        "9D2B6EBDA67E9BB6BE6215959D182041" as parent: valueschemas::GenId;

        "C1EAAA039DA7F486E4A54CC87D42E72C" as task: valueschemas::GenId;
        "61C44E0F8A73443ED592A713151E99A4" as status: valueschemas::ShortString;
    }
}

pub mod orient_state {
    use super::*;
    attributes! {
        "EB687567424358B8780A561EA900513C" as at: valueschemas::NsTAIInterval;
        "6F2D6C7C796B41C2DC7885E7E4D3D750" as local_head: valueschemas::Handle<valueschemas::Blake3, blobschemas::SimpleArchive>;
        "6E6A761126C5101CC69BE185A4B4EC4C" as compass_head: valueschemas::Handle<valueschemas::Blake3, blobschemas::SimpleArchive>;
        "3A58593A230497DEC735E92381C4C522" as relations_head: valueschemas::Handle<valueschemas::Blake3, blobschemas::SimpleArchive>;
        "789078EA4AA95F7B7AD047FF23E04C60" as config_head: valueschemas::Handle<valueschemas::Blake3, blobschemas::SimpleArchive>;
    }
}
