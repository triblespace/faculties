//! The relational half of a compiled field table: what the table's
//! `Order`, `Windows`, `Gaps`, `Links`, mirrors, rewrites and supplies say
//! about the facts the constructions built.
//!
//! One production interpreter, over the frozen construction image and the
//! unit, built on the archive's constructors. The gate's walker and the
//! hand-written importers stay frozen as migration evidence while this
//! replaces them; the spec is the hand-stated fixtures in the tests, each
//! one relational decision with its expected items written out (round-7
//! design, wiki d3f1abd2).

use triblespace::prelude::*;
use triblespace_cxg::Unit;

use super::super::fields::FieldTable;

/// The nodes, witnesses and occurrences of a unit whose facts `image`
/// holds, or why the unit has none.
pub fn build(
    _table: &'static FieldTable,
    _unit: Unit,
    _root: Id,
    _image: &Fragment,
) -> Result<Fragment, String> {
    Err("the relational half is not built yet".to_owned())
}

#[cfg(test)]
mod tests;
