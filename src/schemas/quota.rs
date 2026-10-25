//! Quota schema: provider-reported subscription usage, observed per window.
//!
//! One observation is one immutable entity: which provider, which account
//! (an opaque digest the caller supplies, never a raw account id or email),
//! the window's length, the used percentage exactly as the provider reported
//! it, when that window resets, when the reading was observed, and on which
//! host. The current reading for a `(provider, account, window, bucket)` is a
//! pure maximum over `(observed time, event id)`, where an observation without
//! a bucket is its own bucket value; every older observation stays in the set
//! as history.
//!
//! The observation time is `metadata::created_at`, the canonical point-time
//! attribute Status also uses for its events. The reset time is its own
//! attribute rather than `metadata::expires_at`: a reset is a property of the
//! provider's window, and a generic reader that treats an expired record as
//! disposable would contradict this faculty's rule that an old reading is
//! shown as stale, never dropped.
//!
//! Every id below was minted with `trible genid` on 2026-10-25. Attributes use
//! the explicit-anchor form, so each value encoding participates in identity.
//! `quota::bucket` (`89BD8E51C098EEAFF7D740DB3962882D`) was minted later on
//! 2026-10-25, so two allowances with the same window length stay apart.

use triblespace::macros::id_hex;
use triblespace::prelude::*;

/// Stable extrinsic scope for the Quota collection, named `quota`.
///
/// Minted with `trible genid` on 2026-10-25:
/// `6B6C08A47B334284A56279D961255250`.
pub const DEFAULT_SCOPE_ID: Id = id_hex!("6B6C08A47B334284A56279D961255250");

/// Kind marker carried by every observation.
///
/// Minted with `trible genid` on 2026-10-25:
/// `92B1A5113408368BF20912C6EBFAC6C1`.
pub const KIND_QUOTA_OBSERVATION: Id = id_hex!("92B1A5113408368BF20912C6EBFAC6C1");

/// The Anthropic Claude subscription (Claude Code's `rate_limits`).
///
/// Minted with `trible genid` on 2026-10-25:
/// `99B579DE6F864BDD2007148BCCDAC2A0`.
pub const PROVIDER_CLAUDE: Id = id_hex!("99B579DE6F864BDD2007148BCCDAC2A0");

/// The OpenAI Codex subscription (app-server `account/rateLimits/read`).
///
/// Minted with `trible genid` on 2026-10-25:
/// `A7FA0091FA5E1A63BB8F0449DB8676C0`.
pub const PROVIDER_CODEX: Id = id_hex!("A7FA0091FA5E1A63BB8F0449DB8676C0");

/// The providers this build can author, with the name each is written under
/// (`metadata::name` on the provider entity) and accepted as on the CLI.
///
/// Writing is closed over this table; reading is not. A provider minted by a
/// newer build renders by the name the pile carries for it, or by its id.
pub const PROVIDERS: [(Id, &str); 2] = [(PROVIDER_CLAUDE, "claude"), (PROVIDER_CODEX, "codex")];

pub mod quota {
    use super::*;
    attributes! {
        /// The provider entity this reading is from ([`PROVIDERS`]).
        "68C14DDE02C8C37E7A8E98AB9F6E413C" as provider: inlineencodings::GenId;
        /// Opaque account digest supplied by the caller, e.g. a SHA-256 hex
        /// of the provider's account id. Never the raw id or an address.
        "23224C2886C3147A796D8B8B1BEEDA8A" as account: inlineencodings::Handle<blobencodings::UTF8String>;
        /// Length of the provider's rate-limit window.
        "0B5B1040F92471A6EDB90588D56157C0" as window: inlineencodings::NsDuration;
        /// Used share of the window in percent, as reported; may be
        /// fractional and is not clamped.
        "B2ECF7CAB731B61412B14E5C50202DEB" as used_percent: inlineencodings::F64;
        /// When the provider resets this window (a point interval).
        "4ACB274A8869FD5228142703EC039243" as resets_at: inlineencodings::NsTAIInterval;
        /// Host that made the observation.
        "905CE5FE9C7E6928F98B13DED7A26C71" as host: inlineencodings::Handle<blobencodings::UTF8String>;
        /// Optional provider-side name of the allowance this window belongs
        /// to, e.g. a key of Codex's `rateLimitsByLimitId`. Part of the
        /// grouping key, so two allowances with equal window lengths never
        /// merge; an observation without one groups as "no bucket".
        "89BD8E51C098EEAFF7D740DB3962882D" as bucket: inlineencodings::Handle<blobencodings::UTF8String>;
    }
}
