//! Stable wire schema for immutable encrypted secret versions and recipient wraps.

use crate::encrypted::Encrypted;
use triblespace::core::capability::{capability_action, CapabilityHandle};
use triblespace::macros::{attributes, id_hex};
use triblespace::prelude::*;

/// Stable faculty scope used only to select the self-describing `secrets`
/// source collection through the shared collection-name/configuration layer.
///
/// Minted with `trible genid` on 2026-09-03:
/// `0A33FEA863F9A2F460F98B8F6EE7F0A3`.
pub const DEFAULT_SCOPE_ID: Id = id_hex!("0A33FEA863F9A2F460F98B8F6EE7F0A3");

/// Semantic action of the Secrets key-delivery capability definition.
///
/// Minted with installed `/Users/jp/.cargo/bin/trible genid` on 2026-09-06:
/// `4E350A11267E4E0DA8F547610594D148`.
pub const ACTION_KEY_DELIVERY: Id = id_hex!("4E350A11267E4E0DA8F547610594D148");

/// Stable definition of permission to deliver Secrets data-encryption keys.
///
/// The canonical SimpleArchive of these facts is the capability identity.
/// Collection READ only permits replication of encrypted evidence; it does not
/// imply this capability. Changing these definition facts changes its handle.
pub fn key_delivery_definition() -> Fragment {
    entity! {
        capability_action: ACTION_KEY_DELIVERY,
        triblespace::core::metadata::name: "secrets.key-delivery".to_owned(),
        triblespace::core::metadata::description:
            "Deliver data-encryption keys for Secrets versions by sealing additive envelopes to authorized recipients. Existing envelopes remain usable by possession after authority expires.".to_owned(),
    }
}

/// Exact content handle signed into Secrets key-delivery proof edges.
pub fn key_delivery_capability() -> CapabilityHandle {
    let definition: Blob<blobencodings::SimpleArchive> =
        key_delivery_definition().facts().clone().to_blob();
    definition.get_handle()
}

// These records retain their already-published wire meaning across the
// custody-vault removal. Old vault headers and access envelopes remain inert
// facts; readers only ask for these two shapes.
pub const KIND_SECRET: Id = id_hex!("72B64C9F3644B8016B64820D7F3F23C1");
pub const KIND_WRAP: Id = id_hex!("EB8549BAF679C5D11ECEDB416AAD76E3");

/// An immutable resource descriptor binding one secret body to its delivery policy.
/// Minted with `trible genid` on 2026-09-14.
pub const KIND_SECRET_RESOURCE: Id = id_hex!("8DE676F5445D85A874435C68C9C387E4");

/// Bound recipient-envelope format. Minted with `trible genid` on 2026-09-14.
pub const BOUND_ENVELOPE_MAGIC: [u8; 16] = id_hex!("0AFF57FBB4533A0E74171F2B7893BDC9").raw();

// Structured item markers/roles minted with installed `trible genid`, 2026-10-17.
pub const KIND_ITEM: Id = id_hex!("BA875912DAD2AF6779DC00175D2942AF");
pub const KIND_ITEM_VERSION: Id = id_hex!("103651E249CC495CE716A26C3689B26D");
pub const ROLE_USERNAME: Id = id_hex!("385C89355B86672AA44A5E2BDC1B6336");
pub const ROLE_PASSWORD: Id = id_hex!("09E876C9EC08BD88342D5DC28FB7A2A4");
pub const ROLE_URL: Id = id_hex!("6A5BF630C2CE6387652C0D120B56C415");
pub const ROLE_NOTES: Id = id_hex!("4D5B2D703075E182BCA7B175EAB7F314");
pub const ROLE_CUSTOM: Id = id_hex!("79E38C701F519597B83ADB569A69B5C8");

attributes! {
    /// Password/device envelopes; never a node-recipient DEK wrap. Minted 2026-10-23.
    "7BD1A72C9E4200DD281ED868B222106E" as pub protected_envelope:
        inlineencodings::Handle<blobencodings::RawBytes>;
    // Explicit anchors minted with installed `trible genid`, 2026-10-17.
    /// Public root of a structured encrypted immutable version, not a raw body.
    "E7261A95940626523600E19D172B5563" as pub structured_body:
        inlineencodings::Handle<Encrypted<blobencodings::SimpleArchive>>;
    /// Repeated private field nodes, inside the encrypted archive only.
    "0544A812AB6D17B3576B64A1A663F5CF" as pub item_field: inlineencodings::GenId;
    /// Repeated private attachment nodes, inside the encrypted archive only.
    "47E02C07D70AF3EA1BF425E8A5F54767" as pub item_attachment: inlineencodings::GenId;
    "2EE309C1273C5433CC8C24F7F710DC13" as pub field_role: inlineencodings::GenId;
    "C12F9DBCFE8D377A87B1DE5C12F37A88" as pub field_text:
        inlineencodings::Handle<Encrypted<blobencodings::UTF8String>>;
    "9B6273023FB3BC8716C1FF734382FB3C" as pub attachment_bytes:
        inlineencodings::Handle<Encrypted<blobencodings::RawBytes>>;
    /// A private custom-field label or attachment filename, never a public label.
    "535EF6AFCC5A1985AF7403BF18F0985F" as pub private_name:
        inlineencodings::Handle<Encrypted<blobencodings::UTF8String>>;
    "B686FF41C1BDA50B2406181AF600BBF4" as pub field_integer: inlineencodings::I256BE;
    "7A8D777ADAD7EEBA662445ED2AE6FC08" as pub field_flag: inlineencodings::Boolean;
    "7FC38805FDC9FA4D8449497B298B51BB" unsafe as pub secret_body:
        inlineencodings::Handle<blobencodings::RawBytes>;
    "D17EC6F6A9F9D6B7A3B9A329A9CFC4CC" unsafe as pub wrap_secret:
        inlineencodings::GenId;
    "B30CE37D4DC3CAACC34D946B3D71E37C" unsafe as pub wrap_dek:
        inlineencodings::Handle<blobencodings::RawBytes>;

    /// Ed25519 recipient of one sealed data-encryption key.
    ///
    /// Anchor minted with `trible genid` on 2026-08-23:
    /// `B511AAEB955CD121B6C5E72B3DCEC70F`.
    "B511AAEB955CD121B6C5E72B3DCEC70F" as pub wrap_recipient_key:
        inlineencodings::ED25519PublicKey;

    /// Earliest delivery instant, inclusive. This constrains future envelopes,
    /// never opening an envelope already delivered. Minted 2026-09-14.
    "08CA9343966A09348C3C9B8369021DC0" as pub delivery_not_before:
        inlineencodings::NsTAIInterval;
    /// Latest delivery instant, exclusive. Minted 2026-09-14.
    "17757D913DFEE244444735ED933953CE" as pub delivery_expires_at:
        inlineencodings::NsTAIInterval;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_delivery_definition_is_stable_and_distinct_from_replication() {
        let capability = key_delivery_capability();
        assert_eq!(capability, key_delivery_capability());
        assert_ne!(capability, triblespace::core::collection::read_capability());
        assert_ne!(
            capability,
            triblespace::core::collection::write_capability()
        );
    }

    #[test]
    fn retained_wire_ids_are_stable() {
        for (actual, expected) in [
            (KIND_SECRET, "72B64C9F3644B8016B64820D7F3F23C1"),
            (KIND_WRAP, "EB8549BAF679C5D11ECEDB416AAD76E3"),
            (secret_body.id(), "7FC38805FDC9FA4D8449497B298B51BB"),
            (wrap_secret.id(), "D17EC6F6A9F9D6B7A3B9A329A9CFC4CC"),
            (wrap_dek.id(), "B30CE37D4DC3CAACC34D946B3D71E37C"),
            (wrap_recipient_key.id(), "082D781A1E4C849524EFC07280B42C8A"),
        ] {
            assert_eq!(format!("{actual:X}"), expected);
        }
    }
}
