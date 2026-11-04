//! Media types with canonical parameters, as Files media-type entities.
//!
//! The canonical spelling keeps every piece of information a media type
//! carries and none of its spelling: the type, subtype and parameter names
//! are lowercased (RFC 9110 makes them case-insensitive), parameters are
//! sorted by name (their order means nothing), and each value is kept exactly
//! as decoded, quoted only when it is not a token. So
//! `Audio/L16; rate=24000; channels="1"` is `audio/l16;channels=1;rate=24000`.
//! A parameter-free type is spelled exactly as Files spells it, and so is the
//! same entity Files already names.
//!
//! Nothing is defaulted: an empty or malformed spelling, or a parameter named
//! twice, is an error for the importer to surface.

use anyhow::{anyhow, bail, Result};
use triblespace::core::metadata;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::*;

use crate::schemas::archive_v3 as schema;

/// A parsed media type, in canonical form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaType {
    essence: String,
    parameters: Vec<(String, String)>,
}

fn is_tchar(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

fn is_ows(c: char) -> bool {
    c == ' ' || c == '\t'
}

fn token<'a>(input: &'a str, what: &str, whole: &str) -> Result<(&'a str, &'a str)> {
    let end = input.find(|c: char| !is_tchar(c)).unwrap_or(input.len());
    if end == 0 {
        bail!("media type {whole:?}: expected a {what} token");
    }
    Ok((&input[..end], &input[end..]))
}

fn skip_ows(input: &str) -> &str {
    input.trim_start_matches(is_ows)
}

/// Decode a quoted-string whose opening quote has been consumed.
fn quoted<'a>(input: &'a str, whole: &str) -> Result<(String, &'a str)> {
    let mut value = String::new();
    let mut chars = input.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => return Ok((value, &input[at + 1..])),
            '\\' => match chars.next() {
                Some((_, escaped)) if escaped == '\t' || !escaped.is_control() => {
                    value.push(escaped)
                }
                _ => bail!("media type {whole:?}: bad quoted-pair"),
            },
            c if c == '\t' || !c.is_control() => value.push(c),
            _ => bail!("media type {whole:?}: control character in a quoted value"),
        }
    }
    bail!("media type {whole:?}: unterminated quoted value")
}

impl MediaType {
    /// Parse a media type as a source spells it.
    pub fn parse(spelling: &str) -> Result<Self> {
        let whole = spelling;
        let rest = spelling.trim_matches(is_ows);
        if rest.is_empty() {
            bail!("empty media type");
        }
        let (kind, rest) = token(rest, "type", whole)?;
        let rest = rest
            .strip_prefix('/')
            .ok_or_else(|| anyhow!("media type {whole:?}: expected '/'"))?;
        let (subtype, mut rest) = token(rest, "subtype", whole)?;
        let essence = format!(
            "{}/{}",
            kind.to_ascii_lowercase(),
            subtype.to_ascii_lowercase()
        );
        let mut parameters: Vec<(String, String)> = Vec::new();
        loop {
            rest = skip_ows(rest);
            if rest.is_empty() {
                break;
            }
            rest = rest
                .strip_prefix(';')
                .ok_or_else(|| anyhow!("media type {whole:?}: expected ';'"))?;
            rest = skip_ows(rest);
            // RFC 9110 admits empty parameters (`text/plain;`); they carry
            // nothing.
            if rest.is_empty() || rest.starts_with(';') {
                continue;
            }
            let (name, after) = token(rest, "parameter name", whole)?;
            let after = after
                .strip_prefix('=')
                .ok_or_else(|| anyhow!("media type {whole:?}: expected '=' after {name:?}"))?;
            let (value, after) = match after.strip_prefix('"') {
                Some(inner) => quoted(inner, whole)?,
                None => {
                    let (value, after) = token(after, "parameter value", whole)?;
                    (value.to_owned(), after)
                }
            };
            let name = name.to_ascii_lowercase();
            if parameters.iter().any(|(seen, _)| *seen == name) {
                bail!("media type {whole:?}: parameter {name:?} is given twice");
            }
            parameters.push((name, value));
            rest = after;
        }
        parameters.sort();
        Ok(Self {
            essence,
            parameters,
        })
    }

    /// `type/subtype`, lowercased.
    pub fn essence(&self) -> &str {
        &self.essence
    }

    /// The parameters, sorted by name, with their exact decoded values.
    pub fn parameters(&self) -> &[(String, String)] {
        &self.parameters
    }

    /// The canonical spelling.
    pub fn canonical(&self) -> String {
        let mut out = self.essence.clone();
        for (name, value) in &self.parameters {
            out.push(';');
            out.push_str(name);
            out.push('=');
            if !value.is_empty() && value.chars().all(is_tchar) {
                out.push_str(value);
            } else {
                out.push('"');
                for c in value.chars() {
                    if c == '"' || c == '\\' {
                        out.push('\\');
                    }
                    out.push(c);
                }
                out.push('"');
            }
        }
        out
    }

    fn entity(spelling: String) -> Fragment {
        let mut fragment = Fragment::empty();
        let name = fragment.put::<UTF8String, _>(spelling);
        fragment += entity! {
            metadata::tag: &schema::media_type::KIND,
            metadata::name: name,
        };
        fragment
    }

    /// The media-type entity's identity: `{ tag, name: canonical }`, rooted.
    pub fn core(&self) -> Fragment {
        Self::entity(self.canonical())
    }

    /// Annotations of the entity: its `essence` link, with the essence
    /// entity's own core riding along unexported. Never part of an id.
    pub fn annotations(&self) -> Fragment {
        let root = self
            .core()
            .root()
            .expect("an entity! fragment has one root");
        let essence = Self::entity(self.essence.clone());
        let essence_root = essence.root().expect("an entity! fragment has one root");
        let mut fragment = essence;
        fragment += entity! { ExclusiveId::force_ref(&root) @
            schema::media_type::essence: &essence_root,
        };
        super::unexported(fragment)
    }
}
