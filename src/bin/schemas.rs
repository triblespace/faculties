use anyhow::Result;

use faculties::schema_catalog::cli::{execute, SPEC};

fn main() -> Result<()> {
    faculties::cli::run(&SPEC, execute)
}
