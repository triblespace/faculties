//! The field table as reader: JSON objects opened against a named shape.
//!
//! A [`Shape`] names every member an export's object may hold and what becomes
//! of it. A member the shape does not name, a missing one, a repeated one and a
//! value where only `null` is known each fail the unit, and so does a read
//! member the projection did not take ([`Opened::finish`]). Nothing is left out
//! except by name, and left-out members that held a value are counted by path.

use std::collections::BTreeMap;

use anybytes::{Bytes, View};
use anyhow::{anyhow, bail, Context, Result};
use triblespace::core::import::scanner as sc;

use crate::archive_source;

/// What becomes of one member.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Rule {
    /// The projection takes it.
    Read,
    /// The member's only value.
    Is(&'static str),
    /// Only ever `null`; any other value fails the unit.
    Null,
    /// Left out; the path it is counted under, and why.
    Drop(&'static str, &'static str),
    /// A member that may be absent; present, the rule it names holds.
    Opt(&'static Rule),
}

/// Every member an object of one kind holds.
pub(crate) struct Shape {
    pub(crate) name: &'static str,
    pub(crate) members: &'static [(&'static str, Rule)],
}

/// One object opened against its shape.
pub(crate) struct Opened {
    shape: &'static Shape,
    unread: BTreeMap<&'static str, Bytes>,
}

impl Opened {
    /// The value of a member the shape reads.
    pub(crate) fn take(&mut self, name: &'static str) -> Result<Bytes> {
        self.unread
            .remove(name)
            .ok_or_else(|| anyhow!("{} has no unread member {name:?}", self.shape.name))
    }

    /// The value of a member that may be absent or `null`.
    pub(crate) fn optional(&mut self, name: &'static str) -> Option<Bytes> {
        self.unread.remove(name).filter(|value| !is_null(value))
    }

    /// A string member.
    pub(crate) fn text(&mut self, name: &'static str) -> Result<View<str>> {
        let value = self.take(name)?;
        archive_source::json_str(&value)
            .ok_or_else(|| anyhow!("{}.{name} is not a string", self.shape.name))
    }

    /// A string member that may be absent or `null`.
    pub(crate) fn optional_text(&mut self, name: &'static str) -> Result<Option<View<str>>> {
        self.optional(name)
            .map(|value| {
                archive_source::json_str(&value)
                    .ok_or_else(|| anyhow!("{}.{name} is not a string", self.shape.name))
            })
            .transpose()
    }

    /// An array member's elements.
    pub(crate) fn elements(&mut self, name: &'static str) -> Result<Vec<Bytes>> {
        let value = self.take(name)?;
        archive_source::json_elements(&value)
            .ok_or_else(|| anyhow!("{}.{name} is not an array", self.shape.name))
    }

    /// Every remaining member is kept by a caller that keeps the whole object.
    pub(crate) fn whole(&mut self) {
        self.unread.clear();
    }

    /// Fail if a member the shape reads was not taken.
    pub(crate) fn finish(self) -> Result<()> {
        if let Some(name) = self.unread.keys().next() {
            bail!("{}.{name} was read and not projected", self.shape.name);
        }
        Ok(())
    }
}

pub(crate) fn is_null(value: &Bytes) -> bool {
    value.as_ref() == b"null"
}

/// Whether a left-out value held anything.
fn holds(value: &Bytes) -> bool {
    !matches!(
        value.as_ref(),
        b"null" | b"\"\"" | b"[]" | b"{}" | b"false" | b"\"None\""
    )
}

/// Open `value` against `shape`, counting left-out members that held
/// something in `dropped`.
pub(crate) fn open(
    shape: &'static Shape,
    value: Bytes,
    dropped: &mut BTreeMap<&'static str, usize>,
) -> Result<Opened> {
    if value.first().copied() != Some(b'{') {
        bail!("{} is not an object", shape.name);
    }
    let mut bytes = value;
    let members = sc::object(&mut bytes, Vec::new(), |mut members, key, member| {
        members.push((key, sc::take_value(member)?));
        Ok::<_, sc::ScanError>(members)
    })
    .map_err(|error| anyhow!(error))
    .with_context(|| format!("scan {}", shape.name))?;
    let mut seen = BTreeMap::new();
    for (key, member) in members {
        let Some((name, _)) = shape
            .members
            .iter()
            .find(|(name, _)| name.as_bytes() == key.as_ref())
        else {
            bail!(
                "{} has an unknown member {:?}",
                shape.name,
                String::from_utf8_lossy(key.as_ref())
            );
        };
        if seen.insert(*name, member).is_some() {
            bail!("{} repeats member {name:?}", shape.name);
        }
    }
    let mut unread = BTreeMap::new();
    for (name, rule) in shape.members {
        let (mut rule, mut required) = (rule, true);
        while let Rule::Opt(inner) = rule {
            (rule, required) = (*inner, false);
        }
        let Some(member) = seen.remove(name) else {
            if required {
                bail!("{} has no member {name:?}", shape.name);
            }
            continue;
        };
        match rule {
            Rule::Read => {
                unread.insert(*name, member);
            }
            Rule::Is(expected) => {
                let value = archive_source::json_str(&member);
                if value.as_ref().map(AsRef::as_ref) != Some(*expected) {
                    bail!("{}.{name} is not {expected:?}", shape.name);
                }
            }
            Rule::Null => {
                if !is_null(&member) {
                    bail!(
                        "{}.{name} holds a value, and only null is understood",
                        shape.name
                    );
                }
            }
            Rule::Drop(path, _reason) => {
                if holds(&member) {
                    *dropped.entry(path).or_default() += 1;
                }
            }
            Rule::Opt(_) => unreachable!("the optional rule was resolved above"),
        }
    }
    Ok(Opened { shape, unread })
}
