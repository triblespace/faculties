# Typed encrypted item blobs

`encrypted::Encrypted<T: BlobEncoding>` is a typed XChaCha20-Poly1305 envelope
implemented with RustCrypto `chacha20poly1305 0.10.1`. Its immutable bytes are:

```text
format magic16 | opaque key-ID16 | random nonce24 | child count u64 LE8
explicit ciphertext child handles32*N
ciphertext | tag16
```

The complete visible prefix/reference list, preceded by the expected plaintext
`T::id()` bytes, is AEAD associated data. Recasting a typed handle does not make
wrong-type opening succeed. The key-ID peek is an **unauthenticated routing
hint**, never authority. Readers check framing bounds before slicing or
allocating; opening authenticates the header, type, references and ciphertext.
Plaintext and ciphertext have independent content hashes. No blob store returns
plaintext under a ciphertext handle. Public lengths, key identity and ciphertext
graph topology are intentional disclosures.

`ItemKey::generate` and each seal use fresh OS randomness. `ItemSealer` records
only the ciphertext it produces; children must be sealed before parents.
`finish_facts(private.into_facts())` seals a SimpleArchive root and returns
`Bundle<Encrypted<SimpleArchive>>` carrying that explicit ciphertext inventory.
The inventory is authenticated as a conservative dependency list, not inferred
semantic closure. Existing aligned-word traversal can follow those handles
without keys. Ordinary `entity! { structured_body*: bundle }` and collection
commit copy the bytes without introducing a record format or crypto store shim.

Facts-only finishing intentionally discards private Fragment exports, schema
metafacts and its shared blob store. That store can contain plaintext attribute
descriptions even when all field values have encrypted encodings. Do not forward
it into a public bundle. Every needed out-of-line value, including a private
custom label or filename, must be explicitly sealed first; finishing does not
rewrite plaintext handles or retrieve missing references. Runtime schema data
that must remain readable privately must itself be modeled and sealed explicitly.

Native `item::ItemInput` structs are transient authoring inputs, not stored
catalogs. `item::prepare` creates one fresh immutable version S and DEK, using S
as public key identity. It builds encrypted children, private relational facts,
the encrypted root H, immutable policy descriptor R, and the initial holder wrap
before publication. The existing bound recipient envelope remains exactly
S/H/R/DEK; R comes after H to avoid a hash cycle. Its policy never comes from
appendable union facts. New `structured_body` does not reinterpret historical
`secret_body` or old wraps. `storage::add_item` publishes one ordinary COMMIT.

`item::open` / `SecretsSnapshot::open_item` return every usable body alternative
for one opaque S, identified by H. `open_exact` / `open_item_body` explicitly
select H. An `OpenedItem` exposes native facts and selected typed child opening,
not an eagerly decoded field catalog. Query a root by `metadata::tag: KIND_ITEM`;
Fragment exports are not archived. Repeated fields, extra/unknown facts and
opaque ids remain valid. Missing selected children fail locally. A delivered
item key discloses the whole item; field-selective sharing is not provided.

The configured Faculties API does not return an acquiring reader past its
one-shot store lease: `open_item` returns exact body alternatives,
`open_item_body` returns native facts, and `open_item_child` performs selected
acquisition/decryption before closing the store. `inspect_item` uses the same
lease and emits roles/typed ciphertext handles without opening values. SDK
`OpenedItem<R>` remains appropriate when its caller owns the reader lifetime.

Key/cipher storage and decrypted blob owners zeroize on drop (after the final
shared byte view). This cannot erase caller-created copies, input Blobs,
serialized exports, decoded TribleSet allocations, swap or external captures.

## Current one-time passwords

`OpenedItem::totp(child)` computes a current RFC 6238 code from one explicitly
selected `PrivateText` ciphertext handle. A positive native join must bind that
exact child through an item root's `item_field` / `field_text` relation. Private
labels, attachments and unrelated same-key blobs are not field values. Repeated
fields/roles and unknown facts do not invalidate the selected binding; no role
or iteration-order choice is made. `totp_at(child, unix_seconds)` is the
controlled-time SDK counterpart for callers that own their clock.

The configured operation is `Secrets::item_totp(secret, body, child)`. CLI
`secrets item-totp --secret S --body H --child C` and MCP `secrets_item_totp`
take the same exact immutable selectors and disclose one JSON object:
`code`, `generated_at_unix`, `valid_from_unix`, `valid_until_unix`, `period`,
`digits`, `algorithm`, `issuer`, `account`. Times are integer Unix **seconds**;
the validity interval includes `valid_from_unix` and excludes `valid_until_unix`.
The current clock is sampled after selected acquisition, decryption and parsing.
Native CLI/MCP do not accept a supplied clock. The computation opens no server
file, publishes no credential version/grant and leaves old raw get and explicit
item-export byte contracts unchanged.

A bare padded/unpadded, ASCII-case-insensitive Base32 seed defaults to
SHA1 / 6 digits / 30 seconds, with null issuer/account. Bare seeds ignore only
Unicode whitespace (`char::is_whitespace`) in a temporary zeroizing buffer;
punctuation and zero-width characters are not separators. This computation does
not rewrite the saved field. No invented minimum seed length is imposed beyond
nonempty decoded bytes. An `otpauth://totp/` URI
supports SHA1, SHA256 and SHA512, 6 or 8 digits, and a positive integer period.
URI secrets remain strict Base32, with no whitespace removal.
HOTP/counter, unsupported known parameters, malformed padding/trailing bits,
duplicate known parameters and disagreeing issuer prefix/query are refused.
Unknown URI annotations are ignored. URI identity metadata is decoded and
normalized; provider-specific identity/parameter policies belong to the caller.
The full URI, seed and raw label/query are never returned or echoed in errors.

The code is itself a sensitive short-lived credential; output only follows the
explicit operation. Owned seed and digest buffers use `Zeroizing`. This is
best-effort lifetime hygiene, not a promise to erase parser/HMAC internals,
compiler temporaries, stack, swap or caller-created copies. `Code` has no
`Debug`/`Display` implementation. No schema, encryption or pile format changes
are involved. RFC 6238 vectors and synthetic items cover the computation.

## Minted marker provenance

All following values are exact outputs of installed `trible genid` on
2026-10-17. The format marker names the complete timeless envelope semantics,
not an arbitrary revision counter. `Encrypted<T>` derives a distinct schema
identity from this format marker and T; attribute literals below are anchors,
not unsafe pins of already-published ids.

| Meaning | Minted value |
| --- | --- |
| Encrypted format | `8C711D518C0D28347A76E1B641ED1509` |
| structured_body anchor | `E7261A95940626523600E19D172B5563` |
| KIND_ITEM | `BA875912DAD2AF6779DC00175D2942AF` |
| item_field anchor | `0544A812AB6D17B3576B64A1A663F5CF` |
| item_attachment anchor | `47E02C07D70AF3EA1BF425E8A5F54767` |
| field_role anchor | `2EE309C1273C5433CC8C24F7F710DC13` |
| field_text anchor | `C12F9DBCFE8D377A87B1DE5C12F37A88` |
| attachment_bytes anchor | `9B6273023FB3BC8716C1FF734382FB3C` |
| private_name anchor | `535EF6AFCC5A1985AF7403BF18F0985F` |
| ROLE_USERNAME | `385C89355B86672AA44A5E2BDC1B6336` |
| ROLE_PASSWORD | `09E876C9EC08BD88342D5DC28FB7A2A4` |
| ROLE_URL | `6A5BF630C2CE6387652C0D120B56C415` |
| ROLE_NOTES | `4D5B2D703075E182BCA7B175EAB7F314` |
| ROLE_CUSTOM | `79E38C701F519597B83ADB569A69B5C8` |
| field_integer anchor | `B686FF41C1BDA50B2406181AF600BBF4` |
| field_flag anchor | `7A8D777ADAD7EEBA662445ED2AE6FC08` |
| KIND_ITEM_VERSION | `103651E249CC495CE716A26C3689B26D` |
