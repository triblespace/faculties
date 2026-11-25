# Atomic local release cohorts

`install-release-cohort` installs every executable found directly in one build
directory as one content-verified, versioned generation. A generation path is
write-once by the installer: staging refuses to overwrite it, but the installer
does not claim filesystem immutability or remove owner write permission. Build
with `--workspace` so the
`migrations` binary — which lives in the `faculties-migrations` member crate and
owns the explicit collection-policy transition — is part of the cohort. The
installer refuses a build directory without it, so accidentally omitting
`--workspace` cannot activate an incomplete generation. It copies and hashes
the complete cohort
before atomically replacing `~/.local/lib/faculties/current`; commands in
`~/.local/bin` always pass through that symlink. The `mail` binary is exposed as
`faculties-mail` so the operating system's `mail(1)` is never shadowed.

Keep `~/.local/bin` before directories containing ad-hoc Faculties installs,
especially `~/.cargo/bin`. Activation checks the caller's `PATH` and refuses if
an earlier executable would shadow any command in the staged cohort; it never
deletes or rewrites that executable on the operator's behalf.

The installer refuses dirty Git repositories among the selected local Cargo
dependencies. Each generation carries `Cargo.lock` and a JSON manifest with
the exact source revisions and tree hashes, requested and resolved Cargo
features, verbose Cargo and rustc versions, the allowlisted `RUSTFLAGS` and
`CARGO_INCREMENTAL` environment when present, and SHA-256/size of every binary.
An exact GB10 runner can additionally provide its validated runner identity,
source-cohort digest, and argv receipt through `GB10_EXACT_INVOCATION`.
Untracked siblings and `target/` output outside that source closure do not block
a release.

That `RUSTFLAGS` field is read from the environment only, so flags supplied by
a `.cargo/config.toml` do not appear in it. They are still captured,
one field over: the config file is part of the Faculties tree whose revision
and tree hash the manifest already records. When auditing how a generation was
compiled, read the recorded source revision as well as `environment`; neither
alone is the whole recipe.

Build and inspect without writing anything:

```sh
cargo build --release --workspace --bins --no-default-features
scripts/install-release-cohort target/release \
  --no-default-features --generation "$(date -u +%Y%m%dT%H%M%SZ)-$(git rev-parse --short=12 HEAD)" \
  --dry-run
```

Stage the content-verified generation, validate its copied bytes, and only then
switch the complete command cohort:

```sh
scripts/install-release-cohort target/release \
  --no-default-features --generation <generation> --stage-only
scripts/install-release-cohort --activate-staged <generation> --dry-run
scripts/install-release-cohort --activate-staged <generation>
```

Omit `--stage-only` for a single stage-and-activate operation. Pass the same
`--features a,b` and `--no-default-features` choices used for `cargo build`;
they are recorded as provenance rather than guessed from opaque executables.
Existing commands not managed by this installer are never overwritten.

An activation that reads back removes the generations nothing needs any more,
and prints for each one what happened. Under `~/.local/lib/faculties/releases/`
it keeps

- the current generation and the one current before it (the `previous` symlink
  beside `current`; rollback is the same activation command with that name, and
  activating the same generation twice changes nothing),
- generations staged after the current one,
- generations a process executes, maps, has open, works in, or names in its
  environment or command line, and those a loaded systemd unit or launch agent
  names. A running process keeps its old generation until it is restarted and
  the next prune comes by,
- generations with a file named `.keep`, whose text (who needs it, and why) is
  printed every time. Put one where a stopped launcher or a private
  configuration names a generation by path, which nothing else can see; remove
  it when that moves on.

This is what is running and loaded at that moment, no more. A unit file nothing
has loaded (systemd drops a stopped static unit) and a start after the check do
not show; `.keep` is for those.

If the holders cannot be read at all, everything is kept. What stays unseen are
the working directory and environment of processes of other users; on Linux the
files they run or have open are still asked of the kernel, on macOS they are
not. Nothing but a plain directory with a readable `manifest.json` directly
under `releases/` is ever removed, and no symlink is followed. Each removed
generation leaves its manifest and `Cargo.lock` in
`~/.local/lib/faculties/retired/`. A removal that fails is reported, never fails
the activation, and is finished by the next prune.

```sh
scripts/install-release-cohort --prune-dry-run     # what would go, and who holds the rest
scripts/install-release-cohort --prune-only
scripts/install-release-cohort --activate-staged <generation> --no-prune
```

Activation changes the `current` symlink for **new** processes. A process that
was already running keeps its old executable mapped, even when its command path
now resolves through the new generation. Restart every long-lived Faculties
writer after an activation, especially each `orient wait` watcher. Check the
binary a process is actually executing rather than the current symlink:

```sh
lsof -a -d txt -p <pid>
```

This distinction is load-bearing for append-only piles: an apparently healthy
watcher from an older cohort can keep publishing records under superseded
semantics indefinitely.

Exercise the full install/switch path in an isolated temporary prefix:

```sh
python3 scripts/test_install_release_cohort.py
```
