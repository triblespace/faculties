//! Native encrypted 1Password import receipts. All anchors were minted with
//! installed `trible genid` on 2026-10-17; old JSON-probe receipts are distinct.
use crate::secrets::encrypted::Encrypted;
use triblespace::prelude::*;

pub const KIND_1PASSWORD_IMPORT: Id = id_hex!("4101B50F70312052067291E5DA690757");
/// Separate from ordinary node-readable imports. Minted with installed
/// `trible genid` on 2026-10-23: D4374D8D09F82D060C074E9F9860EE84.
pub const KIND_PROTECTED_1PASSWORD_IMPORT: Id = id_hex!("D4374D8D09F82D060C074E9F9860EE84");

attributes! {
    /// Public equality locator only, never authority to reuse or disclose.
    "ED545D94FA8D2557451BCECAF68DC215" as pub source_fingerprint:
        inlineencodings::Hash<inlineencodings::Blake3>;
    /// These relations occur inside the encrypted native receipt only.
    "A8F8F59C082F18BECDA978324226C5E7" as pub source_archive: inlineencodings::GenId;
    "BAFA69DECC557D52AE34CFA05D9223B1" as pub import_item: inlineencodings::GenId;
    "4E8347E74BC0C09330A4BEA3D103F228" as pub item_ordinal: inlineencodings::U256BE;
    "1ED8FD8F900069ECDF2E1E25E253E84C" as pub import_entry: inlineencodings::GenId;
    "D1AADD45CF02D1781034EED968D9BC9F" as pub imported_body:
        inlineencodings::Handle<Encrypted<blobencodings::SimpleArchive>>;
}
