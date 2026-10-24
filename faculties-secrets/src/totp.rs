//! RFC 6238 over RustCrypto HMAC, consuming only a selected private text child.
//! No seed or complete URI is returned or included in errors. Zeroizing owned
//! seed/digest buffers is best effort, not erasure of parser/hash internals,
//! compiler copies, stack, swap, or caller-created copies.

use anyhow::{anyhow, bail, Result};
use hmac::{Hmac, Mac};
use percent_encoding::percent_decode_str;
use std::time::{SystemTime, UNIX_EPOCH};
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Algorithm {
    Sha1,
    Sha256,
    Sha512,
}

impl Algorithm {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sha1 => "SHA1",
            Self::Sha256 => "SHA256",
            Self::Sha512 => "SHA512",
        }
    }
}

/// Explicit disclosure of one short-lived credential and selected identity
/// metadata. No Debug/Display implementation that might accidentally log it.
pub struct Code {
    pub code: Zeroizing<String>,
    pub generated_at_unix: u64,
    pub valid_from_unix: u64,
    /// Exclusive validity bound, in Unix seconds (not milliseconds).
    pub valid_until_unix: u64,
    pub period: u64,
    pub digits: u8,
    pub algorithm: Algorithm,
    pub issuer: Option<String>,
    pub account: Option<String>,
}

struct Parameters {
    key: Zeroizing<Vec<u8>>,
    algorithm: Algorithm,
    digits: u8,
    period: u64,
    issuer: Option<String>,
    account: Option<String>,
}

fn base32(encoded: &str) -> Result<Zeroizing<Vec<u8>>> {
    let encoded = Zeroizing::new(encoded.to_ascii_uppercase());
    let decoder = if encoded.ends_with('=') {
        data_encoding::BASE32
    } else {
        data_encoding::BASE32_NOPAD
    };
    let key = Zeroizing::new(
        decoder
            .decode(encoded.as_bytes())
            .map_err(|_| anyhow!("invalid TOTP Base32 seed"))?,
    );
    if key.is_empty() {
        bail!("TOTP seed is empty");
    }
    Ok(key)
}

// URI parsers deliberately tolerate malformed escapes; this input boundary does
// not. Unknown annotation names/values remain ignored after structural parsing.
fn valid_escapes(value: &str) -> bool {
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%'
            && !matches!((bytes.next(), bytes.next()), (Some(a), Some(b))
            if a.is_ascii_hexdigit() && b.is_ascii_hexdigit())
        {
            return false;
        }
    }
    true
}

fn parse(value: &str) -> Result<Parameters> {
    if !value.starts_with("otpauth:") {
        // Bare seeds may contain Unicode formatting whitespace, not arbitrary
        // separators. Keep this owned copy zeroizing and URI decoding strict.
        let mut seed = Zeroizing::new(String::with_capacity(value.len()));
        seed.extend(value.chars().filter(|c| !c.is_whitespace()));
        return Ok(Parameters {
            key: base32(&seed)?,
            algorithm: Algorithm::Sha1,
            digits: 6,
            period: 30,
            issuer: None,
            account: None,
        });
    }
    if value.bytes().any(|b| b <= b' ' || b == 0x7f) || !valid_escapes(value) {
        bail!("invalid TOTP URI");
    }
    let uri = url::Url::parse(value).map_err(|_| anyhow!("invalid TOTP URI"))?;
    if uri.scheme() != "otpauth"
        || uri.host_str() != Some("totp")
        || !uri.username().is_empty()
        || uri.password().is_some()
        || uri.port().is_some()
        || uri.fragment().is_some()
        || !uri.path().starts_with('/')
        || uri.path()[1..].contains('/')
    {
        bail!("unsupported or invalid TOTP URI");
    }
    let label = percent_decode_str(&uri.path()[1..])
        .decode_utf8()
        .map_err(|_| anyhow!("invalid TOTP identity"))?;
    if label.is_empty() || label.chars().any(char::is_control) {
        bail!("invalid TOTP identity");
    }
    let (prefix, account) = match label.split_once(':') {
        Some((issuer, account)) => (Some(issuer), account.trim_start_matches(' ')),
        None => (None, label.as_ref()),
    };
    if account.is_empty() || account.contains(':') || prefix == Some("") {
        bail!("invalid TOTP identity");
    }
    let mut seed = None;
    let mut issuer = None;
    let mut algorithm = None;
    let mut digits = None;
    let mut period = None;
    for pair in uri.query().unwrap_or("").split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let Ok(name) = percent_decode_str(name).decode_utf8() else {
            continue;
        };
        if matches!(
            name.as_ref(),
            "secret" | "issuer" | "algorithm" | "digits" | "period" | "counter"
        ) {
            let decoded = percent_decode_str(value)
                .decode_utf8()
                .map_err(|_| anyhow!("invalid UTF-8 in TOTP parameter"))?;
            let _decoded = Zeroizing::new(decoded.into_owned());
        }
    }
    for (name, value) in uri.query_pairs() {
        // Decoded secret copies stay within this computation and wipe on drop.
        let value = Zeroizing::new(value.into_owned());
        match name.as_ref() {
            "secret" => {
                if seed.is_some() {
                    bail!("duplicate TOTP secret parameter");
                }
                seed = Some(base32(&value)?);
            }
            "issuer" => {
                if issuer.is_some() {
                    bail!("duplicate TOTP issuer parameter");
                }
                if value.is_empty() || value.contains(':') || value.chars().any(char::is_control) {
                    bail!("invalid TOTP issuer");
                }
                issuer = Some(value.to_string());
            }
            "algorithm" => {
                if algorithm.is_some() {
                    bail!("duplicate TOTP algorithm parameter");
                }
                algorithm = Some(match value.as_str() {
                    "SHA1" => Algorithm::Sha1,
                    "SHA256" => Algorithm::Sha256,
                    "SHA512" => Algorithm::Sha512,
                    _ => bail!("unsupported TOTP algorithm"),
                });
            }
            "digits" => {
                if digits.is_some() {
                    bail!("duplicate TOTP digits parameter");
                }
                digits = Some(match value.as_str() {
                    "6" => 6,
                    "8" => 8,
                    _ => bail!("unsupported TOTP digits"),
                });
            }
            "period" => {
                if period.is_some() {
                    bail!("duplicate TOTP period parameter");
                }
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    bail!("invalid TOTP period");
                }
                let seconds: u64 = value.parse().map_err(|_| anyhow!("invalid TOTP period"))?;
                if seconds == 0 {
                    bail!("invalid TOTP period");
                }
                period = Some(seconds);
            }
            "counter" => bail!("HOTP counter is unsupported for TOTP"),
            _ => {} // Provider annotations cannot choose cryptographic semantics.
        }
    }
    if let (Some(prefix), Some(issuer)) = (prefix, issuer.as_deref()) {
        if prefix != issuer {
            bail!("TOTP issuer declarations disagree");
        }
    }
    Ok(Parameters {
        key: seed.ok_or_else(|| anyhow!("TOTP secret parameter is missing"))?,
        algorithm: algorithm.unwrap_or(Algorithm::Sha1),
        digits: digits.unwrap_or(6),
        period: period.unwrap_or(30),
        issuer: issuer.or_else(|| prefix.map(str::to_owned)),
        account: Some(account.to_owned()),
    })
}

pub(crate) fn generate(value: &str, unix: Option<u64>) -> Result<Code> {
    let parameters = parse(value)?;
    // Acquisition, decryption and parsing precede the current-time sample.
    let now = match unix {
        Some(now) => now,
        None => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| anyhow!("system clock is before the Unix epoch"))?
            .as_secs(),
    };
    let counter = now / parameters.period;
    let valid_from_unix = now - now % parameters.period;
    let valid_until_unix = valid_from_unix
        .checked_add(parameters.period)
        .ok_or_else(|| anyhow!("TOTP validity window exceeds supported Unix time"))?;
    macro_rules! authenticate {
        ($hash:ty) => {{
            let mut mac = Hmac::<$hash>::new_from_slice(&parameters.key)
                .map_err(|_| anyhow!("TOTP generation failed"))?;
            mac.update(&counter.to_be_bytes());
            Zeroizing::new(mac.finalize().into_bytes().to_vec())
        }};
    }
    let digest = match parameters.algorithm {
        Algorithm::Sha1 => authenticate!(sha1::Sha1),
        Algorithm::Sha256 => authenticate!(sha2::Sha256),
        Algorithm::Sha512 => authenticate!(sha2::Sha512),
    };
    // RFC 4226 dynamic truncation, with RFC 6238's time-based moving factor.
    let offset = (digest[digest.len() - 1] & 15) as usize;
    let binary = u32::from_be_bytes(digest[offset..offset + 4].try_into().unwrap()) & 0x7fff_ffff;
    let number = binary % 10u32.pow(u32::from(parameters.digits));
    Ok(Code {
        code: Zeroizing::new(format!(
            "{number:0width$}",
            width = parameters.digits as usize
        )),
        generated_at_unix: now,
        valid_from_unix,
        valid_until_unix,
        period: parameters.period,
        digits: parameters.digits,
        algorithm: parameters.algorithm,
        issuer: parameters.issuer,
        account: parameters.account,
    })
}

#[cfg(test)]
#[path = "totp_tests.rs"]
mod tests;
