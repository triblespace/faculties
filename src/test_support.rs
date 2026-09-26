use std::path::Path;

use ed25519_dalek::SigningKey;

/// Initialize the durable signer used by a descriptor-policy collection fixture.
pub(crate) fn initialize_open_collection_fixture(
    pile_path: &Path,
    key_path: Option<&Path>,
) -> SigningKey {
    crate::storage::initialize_signer(pile_path, key_path).expect("initialize fixture signer")
}

/// Remove ambient deployment state -- every `TRIBLESPACE_*` variable, `PILE`
/// and `PERSONA` -- so a fixture never reads live collection ids, keys or
/// peers from the shell that runs the tests.
pub(crate) fn clear_ambient_environment() {
    for (name, _) in std::env::vars_os() {
        let ambient = name.to_str().is_some_and(|name| {
            name.starts_with("TRIBLESPACE_") || name == "PILE" || name == "PERSONA"
        });
        if ambient {
            std::env::remove_var(name);
        }
    }
}
