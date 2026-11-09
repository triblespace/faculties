//! A self-check for the WeMM maintenance daemon, off unless
//! [`PROBE_DIR_ENV`] names a directory.
//!
//! At bind, and then after every [`PROBE_EVERY_FOUNDATIONS`] derived
//! foundations, the daemon embeds the fixed [`PROBE_TEXTS`] as one batch in
//! their listed order and compares the raw BF16 output bytes with two
//! baselines: the first probe of this process, and a reference file kept in
//! the probe directory. The reference is written once, by the first process
//! that finds none, and is never overwritten.
//!
//! Both baselines are comparison points, not oracles. A forward early in a
//! process is where earlier observations found disagreeing outputs, so a
//! baseline can itself be the odd one out, and a disagreement does not say
//! which side differs from the usual bytes. Results are therefore only
//! "agreement" or "disagreement". Each one is logged with the identity of
//! both sides, and on disagreement both sides' raw bytes stay on disk. A
//! disagreement never changes, replaces or retracts anything published.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The environment variable that enables the probe and names its directory.
/// Unset or empty, there is no probe.
pub const PROBE_DIR_ENV: &str = "WEMM_PROBE_DIR";

/// Derived foundations between two probes after the one at bind.
pub const PROBE_EVERY_FOUNDATIONS: u64 = 16;

/// Forwards numbered below this are the "early" stratum of a process.
pub const EARLY_FORWARDS: u64 = 3;

/// The probe inputs, embedded as one batch in exactly this order. Their
/// different lengths make the batch pad three of the four rows.
pub const PROBE_TEXTS: [&str; 4] = [
    "The quick brown fox jumps over the lazy dog.",
    "Tomato seedlings want full sun, deep watering and a stake once they reach knee height.",
    "fn main() {\n    let numbers = vec![3, 1, 4, 1, 5, 9, 2, 6];\n    let total: i32 = numbers.iter().sum();\n    println!(\"{total}\");\n}",
    "A lighthouse keeper kept a logbook of every ship that passed during the long winter: their names, the hour they were sighted, the direction of the wind, and whether the fog horn had to be sounded. Most entries were short, but on stormy nights the handwriting grew larger and the notes grew longer.",
];

/// The directory [`PROBE_DIR_ENV`] names, if the probe is enabled.
pub fn dir_from_env_value(value: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    value.filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The SHA-256 of the framed token IDs of every probe row, each row as its
/// length and its IDs, little-endian u32.
pub fn ids_sha256(ids: &[Vec<u32>]) -> String {
    let mut hash = Sha256::new();
    for row in ids {
        hash.update((row.len() as u32).to_le_bytes());
        for id in row {
            hash.update(id.to_le_bytes());
        }
    }
    hex::encode(hash.finalize())
}

/// The SHA-256 of the probe texts, each as its byte length and bytes.
pub fn texts_sha256(texts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for text in texts {
        hash.update((text.len() as u64).to_le_bytes());
        hash.update(text.as_bytes());
    }
    hex::encode(hash.finalize())
}

/// The SHA-256 of the running executable, the identity of every source
/// revision built into it.
pub fn executable_sha256() -> Result<String> {
    let mut file = std::fs::File::open("/proc/self/exe").context("open /proc/self/exe")?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash).context("hash /proc/self/exe")?;
    Ok(hex::encode(hash.finalize()))
}

/// Ordered `key=value` provenance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Provenance(pub Vec<(String, String)>);

impl Provenance {
    pub fn with(mut self, key: &str, value: impl ToString) -> Self {
        self.0.push((key.to_owned(), value.to_string()));
        self
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// One `key=value` per line, in order.
    pub fn text(&self) -> String {
        self.0
            .iter()
            .map(|(k, v)| format!("{k}={}\n", v.replace('\n', " ")))
            .collect()
    }

    pub fn parse(text: &str) -> Self {
        Self(
            text.lines()
                .filter_map(|line| line.split_once('='))
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        )
    }

    /// The first 16 hex digits of the SHA-256 of [`Self::text`].
    pub fn digest(&self) -> String {
        hex::encode(Sha256::digest(self.text().as_bytes()))[..16].to_owned()
    }

    /// `key=value` pairs on one line.
    pub fn line(&self) -> String {
        self.0
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// How two raw outputs of the same probe batch compare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Comparison {
    pub rows: usize,
    pub differing_rows: usize,
    pub differing_bytes: usize,
}

impl Comparison {
    pub fn of(a: &[u8], b: &[u8], rows: usize) -> Self {
        if a.len() != b.len() || rows == 0 || a.len() % rows != 0 {
            return Self {
                rows,
                differing_rows: rows,
                differing_bytes: a.len().max(b.len()),
            };
        }
        let width = a.len() / rows;
        let mut differing_rows = 0;
        let mut differing_bytes = 0;
        for (x, y) in a.chunks(width).zip(b.chunks(width)) {
            let n = x.iter().zip(y).filter(|(p, q)| p != q).count();
            differing_bytes += n;
            differing_rows += usize::from(n > 0);
        }
        Self {
            rows,
            differing_rows,
            differing_bytes,
        }
    }

    pub fn agrees(&self) -> bool {
        self.differing_bytes == 0
    }

    fn words(&self) -> String {
        if self.agrees() {
            "agreement".into()
        } else {
            format!(
                "disagreement in {} of {} rows, {} bytes",
                self.differing_rows, self.rows, self.differing_bytes
            )
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub attempts: u64,
    pub first_disagreements: u64,
    pub reference_disagreements: u64,
}

/// A probe output kept as a baseline: its bytes and where they came from.
#[derive(Clone, Debug)]
struct Baseline {
    raw: Vec<u8>,
    provenance: Provenance,
}

/// The probe bookkeeping of one process. It does no GPU work: the caller
/// runs the forward and hands over the forward number and raw bytes.
pub struct Checker {
    dir: PathBuf,
    identity: Provenance,
    rows: usize,
    stem: String,
    first: Option<Baseline>,
    first_saved: bool,
    reference: Option<Baseline>,
    reference_resolved: bool,
    probes: u64,
    foundations: u64,
    /// `[early, later]`.
    pub strata: [Tally; 2],
}

impl Checker {
    /// `identity` is this process's fixed provenance, and must name at least
    /// `model_root` and `probe_ids_sha256`, which select the reference file.
    pub fn new(dir: &Path, identity: Provenance, rows: usize) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create the WeMM probe directory {}", dir.display()))?;
        let root = identity
            .get("model_root")
            .context("probe identity needs model_root")?;
        let ids = identity
            .get("probe_ids_sha256")
            .context("probe identity needs probe_ids_sha256")?;
        let stem = format!("reference-{root}-{}", &ids[..16.min(ids.len())]);
        Ok(Self {
            dir: dir.to_owned(),
            identity,
            rows,
            stem,
            first: None,
            first_saved: false,
            reference: None,
            reference_resolved: false,
            probes: 0,
            foundations: 0,
            strata: [Tally::default(); 2],
        })
    }

    pub fn identity(&self) -> &Provenance {
        &self.identity
    }

    pub fn reference_path(&self) -> PathBuf {
        self.dir.join(format!("{}.bin", self.stem))
    }

    /// Count one derived foundation; true when a probe is due.
    pub fn foundation_derived(&mut self) -> bool {
        self.foundations += 1;
        self.foundations.is_multiple_of(PROBE_EVERY_FOUNDATIONS)
    }

    /// Compare one probe output and return the lines to log. `forward` is
    /// the probe forward's number in its process, `taken_at` a timestamp.
    pub fn observe(&mut self, forward: u64, raw: Vec<u8>, taken_at: &str) -> Result<Vec<String>> {
        let probe = self.probes;
        self.probes += 1;
        let early = forward < EARLY_FORWARDS;
        let stratum = if early { "early" } else { "later" };
        let provenance = self
            .identity
            .clone()
            .with("probe", probe)
            .with("forward", forward)
            .with("stratum", stratum)
            .with("taken_at", taken_at);
        let mut lines = Vec::new();

        if !self.reference_resolved {
            self.reference_resolved = true;
            lines.push(self.resolve_reference(&raw, &provenance));
        }
        let first = match &self.first {
            None => {
                self.first = Some(Baseline {
                    raw: raw.clone(),
                    provenance: provenance.clone(),
                });
                None
            }
            Some(first) => Some(Comparison::of(&first.raw, &raw, self.rows)),
        };
        let reference = self
            .reference
            .as_ref()
            .map(|reference| Comparison::of(&reference.raw, &raw, self.rows));

        let tally = &mut self.strata[usize::from(!early)];
        tally.attempts += 1;
        let first_disagrees = first.is_some_and(|c| !c.agrees());
        let reference_disagrees = reference.is_some_and(|c| !c.agrees());
        tally.first_disagreements += u64::from(first_disagrees);
        tally.reference_disagreements += u64::from(reference_disagrees);

        let mut saved = Vec::new();
        if first_disagrees || reference_disagrees {
            let name = format!(
                "disagreement-{}-probe{probe}-forward{forward}",
                std::process::id()
            );
            match write_new(&self.dir, &name, &raw, &provenance) {
                Ok(_) => saved.push(format!("{name}.bin")),
                Err(e) => lines.push(format!(
                    "wemm probe {probe}: could not save its output: {e:#}"
                )),
            }
            if first_disagrees && !self.first_saved {
                let first = self
                    .first
                    .as_ref()
                    .expect("a comparison implies a first probe");
                let name = format!("first-{}", std::process::id());
                match write_new(&self.dir, &name, &first.raw, &first.provenance) {
                    Ok(_) => {
                        self.first_saved = true;
                        saved.push(format!("{name}.bin"));
                    }
                    Err(e) => lines.push(format!(
                        "wemm probe {probe}: could not save this process's first probe: {e:#}"
                    )),
                }
            }
        }

        let first_words = match (&first, &self.first) {
            (Some(c), Some(f)) => format!(
                "vs first probe of this process ({}): {}",
                f.provenance.digest(),
                c.words()
            ),
            _ => "this is the first probe of this process".into(),
        };
        let reference_words = match (&reference, &self.reference) {
            (Some(c), Some(r)) => {
                format!("vs reference ({}): {}", r.provenance.digest(), c.words())
            }
            _ => "no reference to compare".into(),
        };
        let [e, l] = self.strata;
        lines.push(format!(
            "wemm probe {probe}: forward {forward} ({stratum}), identity {}; {first_words}; {reference_words}{}; early {}/{}/{} later {}/{}/{} (attempts/first disagreements/reference disagreements)",
            self.identity.digest(),
            if saved.is_empty() { String::new() } else { format!("; saved {}", saved.join(", ")) },
            e.attempts, e.first_disagreements, e.reference_disagreements,
            l.attempts, l.first_disagreements, l.reference_disagreements,
        ));
        Ok(lines)
    }

    /// Load the reference, or write this probe as the reference when there
    /// is none. An existing reference is never overwritten.
    fn resolve_reference(&mut self, raw: &[u8], provenance: &Provenance) -> String {
        let path = self.reference_path();
        match write_new(&self.dir, &self.stem, raw, provenance) {
            Ok(true) => {
                self.reference = Some(Baseline {
                    raw: raw.to_vec(),
                    provenance: provenance.clone(),
                });
                return format!(
                    "wemm probe: wrote the reference {} from this probe ({})",
                    path.display(),
                    provenance.digest()
                );
            }
            Ok(false) => {}
            Err(e) => return format!("wemm probe: no reference this process: {e:#}"),
        }
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(e) => {
                return format!(
                    "wemm probe: no reference this process: read {}: {e}",
                    path.display()
                )
            }
        };
        let provenance = std::fs::read_to_string(path.with_extension("provenance"))
            .map(|text| Provenance::parse(&text))
            .unwrap_or_default();
        let line = format!(
            "wemm probe: reference {} ({}) taken with {}",
            path.display(),
            provenance.digest(),
            if provenance.0.is_empty() {
                "no provenance file".into()
            } else {
                provenance.line()
            }
        );
        self.reference = Some(Baseline { raw, provenance });
        line
    }
}

/// Write `<name>.bin` and `<name>.provenance` in `dir` only if `<name>.bin`
/// does not exist. Each file is written whole under a temporary name and then
/// hard-linked into place, which fails rather than replaces. Returns false
/// when `<name>.bin` already existed.
fn write_new(dir: &Path, name: &str, raw: &[u8], provenance: &Provenance) -> Result<bool> {
    let bin = dir.join(format!("{name}.bin"));
    let meta = dir.join(format!("{name}.provenance"));
    let tmp = |ext: &str| dir.join(format!(".{name}.{ext}.tmp-{}", std::process::id()));
    let write = |path: &Path, bytes: &[u8]| -> Result<()> {
        let mut file =
            std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    };
    let (tmp_bin, tmp_meta) = (tmp("bin"), tmp("provenance"));
    write(&tmp_meta, provenance.text().as_bytes())?;
    write(&tmp_bin, raw)?;
    let linked = match std::fs::hard_link(&tmp_bin, &bin) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_bin);
            let _ = std::fs::remove_file(&tmp_meta);
            return Err(e).with_context(|| format!("link {}", bin.display()));
        }
    };
    if linked {
        if let Err(e) = std::fs::hard_link(&tmp_meta, &meta) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                let _ = std::fs::remove_file(&tmp_bin);
                let _ = std::fs::remove_file(&tmp_meta);
                return Err(e).with_context(|| format!("link {}", meta.display()));
            }
        }
    }
    let _ = std::fs::remove_file(&tmp_bin);
    let _ = std::fs::remove_file(&tmp_meta);
    Ok(linked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(mode: &str) -> Provenance {
        Provenance::default()
            .with("model_root", "85B6ACD39E6CB93EE8BA31FDEF6C4F4F")
            .with("probe_ids_sha256", "0123456789abcdef0123")
            .with("weights", mode)
    }

    fn rows(fill: u8) -> Vec<u8> {
        vec![fill; 4 * 16]
    }

    #[test]
    fn the_reference_is_written_once_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = Checker::new(dir.path(), identity("pile-aliased"), 4).unwrap();
        let lines = a.observe(0, rows(1), "t0").unwrap();
        assert!(lines[0].contains("wrote the reference"), "{lines:?}");
        let path = a.reference_path();
        assert_eq!(std::fs::read(&path).unwrap(), rows(1));
        let written = std::fs::read_to_string(path.with_extension("provenance")).unwrap();
        assert!(written.contains("forward=0\n") && written.contains("weights=pile-aliased\n"));

        // A second process with different bytes reads it and leaves it alone.
        let mut b = Checker::new(dir.path(), identity("device-copy"), 4).unwrap();
        let lines = b.observe(0, rows(2), "t1").unwrap();
        assert!(
            lines[0].contains("taken with") && lines[0].contains("weights=pile-aliased"),
            "{lines:?}"
        );
        assert!(
            lines[1].contains("vs reference")
                && lines[1].contains("disagreement in 4 of 4 rows, 64 bytes"),
            "{lines:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), rows(1));
        assert_eq!(
            std::fs::read_to_string(path.with_extension("provenance")).unwrap(),
            written
        );
        // Both sides of that disagreement are on disk: the reference, and
        // this probe's output with its provenance.
        let saved = dir.path().join(format!(
            "disagreement-{}-probe0-forward0.bin",
            std::process::id()
        ));
        assert_eq!(std::fs::read(&saved).unwrap(), rows(2));
        assert!(std::fs::read_to_string(saved.with_extension("provenance"))
            .unwrap()
            .contains("weights=device-copy\n"));
        // No temporary file is left behind.
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with('.')));
    }

    #[test]
    fn strata_and_baselines_are_counted_per_forward() {
        let dir = tempfile::tempdir().unwrap();
        let mut checker = Checker::new(dir.path(), identity("pile-aliased"), 4).unwrap();
        checker.observe(0, rows(1), "t0").unwrap();
        // One differing byte in row 2, early.
        let mut odd = rows(1);
        odd[2 * 16 + 3] = 9;
        let lines = checker.observe(2, odd, "t1").unwrap();
        let line = lines.last().unwrap();
        assert!(line.contains("forward 2 (early)"), "{line}");
        assert!(line.contains("vs first probe of this process"), "{line}");
        assert!(
            line.contains("disagreement in 1 of 4 rows, 1 bytes"),
            "{line}"
        );
        assert!(
            !line.contains("corrupt") && !line.contains("correct"),
            "{line}"
        );
        // Later forwards agree.
        for forward in [3, 40] {
            let line = checker
                .observe(forward, rows(1), "t")
                .unwrap()
                .pop()
                .unwrap();
            assert!(
                line.contains("(later)") && line.contains("agreement; vs reference"),
                "{line}"
            );
        }
        assert_eq!(
            checker.strata,
            [
                Tally {
                    attempts: 2,
                    first_disagreements: 1,
                    reference_disagreements: 1
                },
                Tally {
                    attempts: 2,
                    first_disagreements: 0,
                    reference_disagreements: 0
                },
            ]
        );
        // The first probe of the process was saved beside the disagreement,
        // once.
        let pid = std::process::id();
        assert_eq!(
            std::fs::read(dir.path().join(format!("first-{pid}.bin"))).unwrap(),
            rows(1)
        );
        assert!(dir
            .path()
            .join(format!("disagreement-{pid}-probe1-forward2.bin"))
            .exists());
    }

    #[test]
    fn a_probe_is_due_every_named_number_of_foundations() {
        let dir = tempfile::tempdir().unwrap();
        let mut checker = Checker::new(dir.path(), identity("pile-aliased"), 4).unwrap();
        let due: Vec<u64> = (1..=3 * PROBE_EVERY_FOUNDATIONS)
            .filter(|_| checker.foundation_derived())
            .collect();
        assert_eq!(
            due,
            vec![
                PROBE_EVERY_FOUNDATIONS,
                2 * PROBE_EVERY_FOUNDATIONS,
                3 * PROBE_EVERY_FOUNDATIONS
            ]
        );
    }

    #[test]
    fn comparison_counts_rows_and_bytes() {
        assert!(Comparison::of(&rows(1), &rows(1), 4).agrees());
        let c = Comparison::of(&rows(1), &rows(1)[..32], 4);
        assert_eq!((c.differing_rows, c.differing_bytes), (4, 64));
    }

    #[test]
    fn the_probe_is_off_unless_its_directory_is_named() {
        use std::ffi::OsStr;
        assert_eq!(dir_from_env_value(None), None);
        assert_eq!(dir_from_env_value(Some(OsStr::new(""))), None);
        assert_eq!(
            dir_from_env_value(Some(OsStr::new("/var/x"))),
            Some(PathBuf::from("/var/x"))
        );
    }

    #[test]
    fn identities_hash_inputs_by_length_and_content() {
        assert_ne!(
            ids_sha256(&[vec![1, 2], vec![3]]),
            ids_sha256(&[vec![1], vec![2, 3]])
        );
        assert_ne!(texts_sha256(&["ab", "c"]), texts_sha256(&["a", "bc"]));
        let p = Provenance::default().with("a", 1).with("b", "x y");
        assert_eq!(Provenance::parse(&p.text()), p);
        assert_eq!(p.line(), "a=1 b=x y");
    }
}
