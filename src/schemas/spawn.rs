//! Spawn schema: task zooids started from a brief, and each launch of one.
//!
//! A spawn IS its brief. Its intrinsic identity is the kind tag and the brief
//! handle, so relaunching an identical brief names the same spawn and a
//! changed brief names a new one. Its name (the first 32 bits
//! of the brief handle as two proquints) is an annotation on that entity: a
//! convenience for people and for Relations, never a key anything validates.
//!
//! A run is one launch of a spawn. Its intrinsic identity is the spawn it
//! launched and its start time; everything else a launch knows (the unit,
//! the working directory, the effort, the persona it ran as, who spawned it,
//! the rendered prompt) annotates the run, and settlement later adds the end
//! time, the exit and the report to the same run. Two launches of one brief
//! are two runs, so their exits never mix.
//!
//! Times reuse `metadata::started_at` and `metadata::finished_at`; the
//! spawner reuses the shared author relation `archive::author`.
//!
//! Every id below was minted with `trible genid` on 2026-11-16, unless its
//! documentation names a later day. Attributes use
//! the explicit-anchor form, so each value encoding participates in identity.

use triblespace::macros::id_hex;
use triblespace::prelude::*;

/// Stable extrinsic scope of the Spawn collection, named `spawn`.
///
/// Minted with `trible genid` on 2026-11-16:
/// `1570DEB13AA10F1631E66D937A731C55`.
pub const DEFAULT_SCOPE_ID: Id = id_hex!("1570DEB13AA10F1631E66D937A731C55");

/// Kind marker of a spawn (one brief).
///
/// Minted with `trible genid` on 2026-11-16:
/// `8F02DF7BB6C50FF3F3BC5024329B4932`.
pub const KIND_SPAWN: Id = id_hex!("8F02DF7BB6C50FF3F3BC5024329B4932");

/// Kind marker of a run (one launch of a spawn).
///
/// Minted with `trible genid` on 2026-11-16:
/// `A44B774609F22742FEE22457D8268CFE`.
pub const KIND_RUN: Id = id_hex!("A44B774609F22742FEE22457D8268CFE");

/// Kind marker of a run request: a record asking one host's keeper to start
/// a run of a zooid there (`zooid request`, `zooid keep`).
///
/// Minted with `trible genid` on 2026-11-17:
/// `C19ED05A4C53CDA5645CE2D8897B6119`.
pub const KIND_REQUEST: Id = id_hex!("C19ED05A4C53CDA5645CE2D8897B6119");

/// Kind marker of an outcome: what a keeper observed and did about one
/// request. Its intrinsic core is the request and the outcome kind, so the
/// same outcome recorded again is the same entity.
///
/// Minted with `trible genid` on 2026-11-17:
/// `BB838A2D0051B0C5EDD7264AA79A9E27`.
pub const KIND_OUTCOME: Id = id_hex!("BB838A2D0051B0C5EDD7264AA79A9E27");

/// Outcome: the keeper admitted the request and is starting its run.
/// Minted with `trible genid` on 2026-11-17.
pub const OUTCOME_ACCEPTED: Id = id_hex!("DF057A01A2C09442B42BEBB61960C4DD");
/// Outcome: the request's unit is running. Minted 2026-11-17.
pub const OUTCOME_RUNNING: Id = id_hex!("2EB86EBD334839E3CE5AE52120D935B1");
/// Outcome: the keeper will not run the request; the reason says why.
/// Minted 2026-11-17.
pub const OUTCOME_REFUSED: Id = id_hex!("E11BF1D92608F66308A21E7FCB07B65B");
/// Outcome: the request's run ended and settlement recorded the end. A
/// process exit is not goal completion; this says only that the task's run is
/// over. Minted 2026-11-17.
pub const OUTCOME_TERMINAL: Id = id_hex!("A23987DE98AE18530D7CC04A2E78B432");

pub mod spawn {
    use super::*;

    attributes! {
        /// The brief, verbatim: the spawn's identity core and its prompt.
        "49833E5EB9DE7F828EB81CA8072AF599" as brief: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The zooid's name, e.g. `lusab-babad`. Annotation only.
        "B0AF09047B9A884057D1D97DD445A638" as name: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The spawn a run launched.
        "CDC9578C8CA31789C5EA5EBDD7897A35" as of: inlineencodings::GenId;
        /// The Relations persona the run's zooid acted as.
        "2BCEF6826BC5474FCE933A22DB55ACE5" as persona: inlineencodings::GenId;
        /// The systemd user unit the run was started in.
        "11B1B5FFE49B3131764DF6F64A3B5E0A" as unit: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The directory the zooid started in, on the spawning host.
        "94D1C5DA8C9B004162DBB2D8B0FDDF76" as workdir: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The Claude effort level the run was started with.
        "D819F708F4213FA6188A80F778C33C78" as effort: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The whole opening context fed to the zooid: wake, tail, preamble, brief.
        "9E76A94B167864C6C61D1FA7B5369FF2" as prompt: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The main process's exit code, when it exited rather than being killed.
        "CEBA41649EE59E5026D9C77A9D043846" as exit_code: inlineencodings::U256BE;
        /// systemd's verdict on the unit (`success`, `exit-code`, `signal`,
        /// `oom-kill`, `timeout`, ...), exactly as it reported it.
        "05D4D795629D5F72AAC6205C62320DDD" as result: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The signal that ended the main process, when one did (`KILL`, `TERM`, ...).
        "17CFAFF638D79C65700F04D24EE60411" as signal: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The zooid's final reply: claude's stdout.
        "B7536194BBE4B9F40322AE7DB2107528" as report: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The message settlement sent the spawner, once it was sent. A run
        /// that has one is not messaged again by a repeated settlement.
        /// Minted 2026-11-16.
        "3DAD42A1E496E14BC3DE946F835C0144" as message: inlineencodings::GenId;
        /// Why a settlement step failed (messaging the spawner, retiring the
        /// persona). The persona stays live while messaging has not succeeded.
        /// Minted 2026-11-16.
        "77A6F5D7FEFB8A3E81816D9405C73AC3" as settle_failure: inlineencodings::Handle<blobencodings::UTF8String>;
        /// Why the run's recollection is not tinted by its topic: the topic
        /// query failed and the untinted memory tail was used instead.
        /// Minted 2026-11-16.
        "B5BD0429EB1C43D6F4135BA6BEECF09D" as untinted: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The optional semantic scorer failed and the worker used its lexical cue.
        /// The value describes the failure. Minted with `trible genid` on 2026-11-24.
        "41333785A26BD7647F89E83BE1979357" as lexical_fallback: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The Compass goal a spawn's brief was rendered from (`zooid goal`).
        /// Minted 2026-11-16.
        "FD3241C45361C64F87ED5A597F772BFF" as goal: inlineencodings::GenId;
        /// A Compass note or status event the brief was rendered from: with
        /// [`goal`], the exact goal revision. Minted 2026-11-16.
        "06295B697E2572E5FBEF9EC4C6A51EA1" as goal_revision: inlineencodings::GenId;
        /// The colony a zooid's name was taken from: the Relations group
        /// marked with [`crate::schemas::relations::KIND_COLONY`] that held
        /// its spawner, or the origin colony of the zooid that spawned it.
        /// Recorded once, when the name is chosen; a child spawned by this
        /// zooid inherits it. It grants no membership and no authority.
        /// Minted with `trible genid` on 2026-11-21.
        "B0F4A5704A5D30365828287F93D41B9C" as origin_colony: inlineencodings::GenId;
        /// The Compass note the zooid added to its goal when the run launched.
        /// Minted 2026-11-16.
        "719C02B24197877361926DEA741AE59C" as goal_note: inlineencodings::GenId;
        /// Which agent ran the zooid (`claude`, `codex`). Never part of the name.
        /// Minted 2026-11-16.
        "793766C6AE1354A9EBD0036458FD06D2" as provider: inlineencodings::Handle<blobencodings::UTF8String>;
        /// Why that provider was chosen: the quota readings that decided an
        /// `auto` choice, or that it was asked for. Minted 2026-11-16.
        "055AE3D781937D52DAA5E9EF2E1B3770" as provider_reason: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The model the agent was told to use (`claude --model`, `codex -m`).
        /// Minted 2026-11-17.
        "8D412C29A201ABB6CD57E13790038526" as model: inlineencodings::Handle<blobencodings::UTF8String>;
        /// Why the run names no [`model`]: none was configured for its
        /// provider, so the agent chose its own default. Minted 2026-11-17.
        "2FC5D6F2A0327CC8A9EB4918F74BF65D" as model_unset: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The spawn a request asks to run. Minted 2026-11-17.
        "5AB17149BAD55FC21BAC6EC5393E767A" as zooid: inlineencodings::GenId;
        /// The node key of the one host a request is addressed to, as custody
        /// and the network name peers. Minted 2026-11-17.
        "ECE6C8630D9BCEB5615FFE89FFB411F2" as host: inlineencodings::ED25519PublicKey;
        /// A readable name for a request's host (`sky`). Annotation only:
        /// the host is its key. Minted 2026-11-17.
        "7A3D448311C0572AB22E74C5F87FA7BC" as host_label: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The run a request continues, when it is a move. Minted 2026-11-17.
        "95FB4132EA689E333A81FD35CAB47CF8" as previous: inlineencodings::GenId;
        /// Text delivered once, after the brief, in the prompt of the run that
        /// serves a request. Minted 2026-11-17.
        "0775A8E50E6CFAC1091331B441D1A354" as handoff: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The request a run serves, or an outcome is about. Minted 2026-11-17.
        "AC8D99BE158EBBE0D88236E01D1ABBFB" as request: inlineencodings::GenId;
        /// The archived node a local run's zooid continued its history from
        /// (Drive's `--native-resume`), as the launch found it in the archive.
        /// Absent when the zooid had no history there. Minted 2026-11-22.
        "D253320ACDE8B68B6CD46D0E98F95222" as resumes: inlineencodings::GenId;
        /// Which outcome an outcome entity is (`OUTCOME_*`). Minted 2026-11-17.
        "8BE0475206CD7D95B03FE7D1EB27609A" as outcome: inlineencodings::GenId;
        /// Why a keeper refused a request, or what it observed beside an
        /// outcome. Minted 2026-11-17.
        "3C4380BFBC6A0149F6F62618FF1E3CC8" as reason: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The run an outcome refers to. Minted 2026-11-17.
        "3AE1B0C884653F349C65776191733A1E" as outcome_run: inlineencodings::GenId;
        /// Input tokens the provider served from its prefix cache, out of
        /// the run's input tokens (`imagine::input_tokens`). Recorded at
        /// settlement for runs whose agent reports usage (`mistral`).
        /// Minted with `trible genid` on 2026-11-17.
        "DF911DB9B6BB40BECDA671498382774F" as cached_input_tokens: inlineencodings::U256BE;
        /// What the run cost in US dollars, as its agent estimated it from
        /// the prices in the run's own configuration: an estimate, not the
        /// provider's bill. Recorded at settlement (`mistral`, from vibe's
        /// `export.json`). Minted with `trible genid` on 2026-11-17.
        "26501A21D1CA70A5C8ADE843317A9D22" as cost_usd: inlineencodings::F64;
        /// The share of the worker's context window, in percent, asked for
        /// the whole cover (memories, beliefs, goals): a best-effort wish,
        /// not an allocation (`zooid spawn --cover`). On a run and on a
        /// request. Minted with `trible genid` on 2026-11-20, like every
        /// attribute below.
        "2760EF854F5B4200EE6DD3FB5C025486" as cover_percent: inlineencodings::F64;
        /// The worker model's effective context window in tokens, as the
        /// launch sized the cover against it.
        "A669BF2422A02CD0A0615775AB1D1637" as window: inlineencodings::U256BE;
        /// Where [`window`] came from: the `--window` flag, the provider's
        /// own configuration, or a value recorded from an earlier run (with
        /// its date). When no source gave one, why not.
        "A11B538EA9D1D4F2363B1F07379F4775" as window_source: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The characters-per-token estimate the launch converted with. An
        /// estimate, not a count.
        "4E184BB4C14254B12CF22C62944B8937" as chars_per_token: inlineencodings::F64;
        /// How many measured runs [`chars_per_token`] rests on. Absent for a
        /// stated default.
        "7F3CB1F3E6C55E676AF0271AD23BCBC5" as chars_per_token_n: inlineencodings::U256BE;
        /// What [`chars_per_token`] is: which measurement, or that it is a
        /// default for a model with no calibration.
        "04848666DB77C23FB4AA0717267DD642" as chars_per_token_basis: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The whole cover's target in characters: [`cover_percent`] of
        /// [`window`], converted at [`chars_per_token`].
        "19A16D1E322B1438D983CDE566780B80" as cover_target_chars: inlineencodings::U256BE;
        /// The memory characters the cover was rendered with.
        "C0F0039FA7720E7865E87C12E07045A3" as memory_budget: inlineencodings::U256BE;
        /// The characters of the rendered wake: memories, beliefs, goals.
        "00688FBD690AFBD0ADD4CCBF10C56244" as cover_chars: inlineencodings::U256BE;
        /// The characters of the wake rendered with no memory budget: the
        /// beliefs and goals, which always render whole.
        "06802B5A38B326C85E72B7A85E143CCC" as fixed_chars: inlineencodings::U256BE;
        /// The estimated tokens of the whole opening prompt, the provider's
        /// harness overhead included. An estimate.
        "728DC3993E9CF20275552F36637DCA16" as prompt_tokens_estimate: inlineencodings::U256BE;
        /// The fraction of [`window`] the launch left for the work when it
        /// checked the estimate against the window.
        "DDFD4ABA01A3FBD1C19093DC2CFC8D2E" as headroom: inlineencodings::F64;
        /// Why and how the memory part was rendered again with a smaller
        /// budget: the first budget and estimate, the second, and whether
        /// even that is estimated not to fit.
        "32CA9ABDC02633289E1D46F01356C1F2" as cover_reduction: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The cover sizing in words, as the launch printed it.
        "0C2D920B25D94AAC4BF00E9B9C967E8D" as cover_sizing: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The context window the provider itself reported for the run's
        /// model, recorded at settlement (claude's result).
        /// A later launch of the same provider and model may size against it.
        "4C6F000230728ED25E33E3DC15BE3C03" as context_window: inlineencodings::U256BE;
        /// The input tokens of the run's first model call as the provider
        /// reported them, cached and uncached together: harness overhead,
        /// prompt and cover in one total, not a count of the cover.
        /// Recorded at settlement.
        "A77C832D269A2AEEAB5E3A2CF5C2BCC7" as first_call_input_tokens: inlineencodings::U256BE;
    }
}
