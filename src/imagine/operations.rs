use anybytes::Bytes;
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use triblespace::prelude::Id;

use crate::storage::Storage;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Variant {
    #[default]
    Klein,
    Dev,
}
impl Variant {
    pub fn default_steps(self) -> usize {
        match self {
            Self::Klein => 8,
            Self::Dev => 28,
        }
    }
    fn repo(self) -> &'static str {
        match self {
            Self::Klein => "models--black-forest-labs--FLUX.2-klein-4B",
            Self::Dev => "models--black-forest-labs--FLUX.2-dev",
        }
    }
    fn pile_name(self) -> &'static str {
        match self {
            Self::Klein => "flux_klein.pile",
            Self::Dev => "flux_dev.pile",
        }
    }
}

/// Options of the local FLUX.2 pipeline.
#[derive(Clone, Debug)]
pub struct FluxOptions {
    pub prompt: String,
    pub variant: Variant,
    pub steps: Option<usize>,
    pub seed: u64,
    pub width: usize,
    pub height: usize,
    pub guidance: f32,
}
impl FluxOptions {
    /// Reject malformed or accidentally enormous jobs before loading weights.
    /// These are request limits, not a guarantee that a device has enough RAM.
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.prompt.trim().is_empty(), "prompt must not be empty");
        for (label, dimension) in [("width", self.width), ("height", self.height)] {
            ensure!(
                (16..=4096).contains(&dimension) && dimension % 16 == 0,
                "{label} must be a multiple of 16 between 16 and 4096"
            );
        }
        ensure!(
            (1..=200).contains(&self.steps.unwrap_or(self.variant.default_steps())),
            "steps must be between 1 and 200"
        );
        ensure!(
            self.guidance.is_finite() && self.guidance >= 0.0,
            "guidance must be finite and nonnegative"
        );
        Ok(())
    }
}

/// Trusted configuration, captured once by a frontend, not an MCP argument.
#[derive(Clone, Debug)]
pub struct ModelSources {
    pub hub: Option<PathBuf>,
    pub weights_root: PathBuf,
    pub weights_override: Option<PathBuf>,
}
impl ModelSources {
    /// Reads configuration only: no directory scan, model load, or GPU init.
    pub fn from_environment() -> Self {
        Self {
            hub: std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".cache/huggingface/hub")),
            weights_root: crate::model_dir(),
            weights_override: std::env::var_os("FLUX_PILE").map(PathBuf::from),
        }
    }
    pub fn weights(&self, variant: Variant) -> PathBuf {
        self.weights_override
            .clone()
            .unwrap_or_else(|| self.weights_root.join(variant.pile_name()))
    }
    pub fn model_directory(&self, variant: Variant) -> Result<PathBuf> {
        let snapshots = self
            .hub
            .as_ref()
            .context("HOME/Hugging Face hub is not configured")?
            .join(variant.repo())
            .join("snapshots");
        // Stable selection when several cached snapshots are present. This is
        // only configuration/tokenizer selection, never arbitrary weight files.
        let mut candidates = std::fs::read_dir(&snapshots)
            .with_context(|| format!("read model snapshots at {}", snapshots.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        candidates.sort();
        candidates
            .into_iter()
            .find(|path| path.join("transformer/config.json").is_file())
            .with_context(|| {
                format!(
                    "no snapshot with transformer/config.json under {}",
                    snapshots.display()
                )
            })
    }
}

#[derive(Clone, Debug)]
pub struct GeneratedImage {
    pub png: Bytes,
    pub width: u32,
    pub height: u32,
    pub elapsed: std::time::Duration,
    pub luminance_mean: f64,
    pub luminance_stddev: f64,
}
impl GeneratedImage {
    pub fn from_rgb(image: image::RgbImage, elapsed: std::time::Duration) -> Result<Self> {
        ensure!(
            image.width() > 0 && image.height() > 0,
            "generated image is empty"
        );
        let (mut sum, mut squared) = (0.0_f64, 0.0_f64);
        for pixel in image.pixels() {
            let luminance = 0.299 * f64::from(pixel[0])
                + 0.587 * f64::from(pixel[1])
                + 0.114 * f64::from(pixel[2]);
            sum += luminance;
            squared += luminance * luminance;
        }
        let count = f64::from(image.width()) * f64::from(image.height());
        let mean = sum / count;
        let mut encoded = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut encoded, image::ImageFormat::Png)
            .context("encode generated PNG")?;
        Ok(Self {
            png: encoded.into_inner().into(),
            width: image.width(),
            height: image.height(),
            elapsed,
            luminance_mean: mean,
            luminance_stddev: (squared / count - mean * mean).max(0.0).sqrt(),
        })
    }
    pub fn diagnostic(&self) -> String {
        let mut text = format!(
            "{}x{} luminance mean={:.1} std={:.2} ({:.1}s)",
            self.width,
            self.height,
            self.luminance_mean,
            self.luminance_stddev,
            self.elapsed.as_secs_f64()
        );
        if self.luminance_stddev < 1.0 {
            text.push_str("\nWARNING: image is nearly flat; check steps, seed, and model.");
        }
        text
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(path, self.png.as_ref())
            .with_context(|| format!("write PNG to {}", path.display()))
    }
}

/// The local FLUX.2 generator. Needs the `imagine` cargo feature and weights.
#[derive(Clone, Debug)]
pub struct Flux {
    pub sources: ModelSources,
}
impl Flux {
    pub fn new(sources: ModelSources) -> Self {
        Self { sources }
    }

    pub fn generate(&self, options: &FluxOptions) -> Result<GeneratedImage> {
        options.validate()?;
        #[cfg(not(feature = "imagine"))]
        anyhow::bail!("image generation requires a build with the `imagine` feature");
        #[cfg(feature = "imagine")]
        {
            use mary::models::flux::pipeline::{Flux2Pipeline, FluxWeights, ModelVariant};
            let variant = match options.variant {
                Variant::Klein => ModelVariant::Klein,
                Variant::Dev => ModelVariant::Dev,
            };
            let directory = self.sources.model_directory(options.variant)?;
            ensure!(
                ModelVariant::detect(&directory)? == variant,
                "FLUX configuration does not match requested variant"
            );
            super::configuration::preflight(&directory, options.variant)?;
            let pile = self.sources.weights(options.variant);
            let weights = crate::model_storage::with_snapshot(&pile, "FLUX", |snapshot| {
                FluxWeights::from_snapshot(snapshot.clone(), variant)
                    .context("select native FLUX components")
            })?;
            let started = std::time::Instant::now();
            let image = Flux2Pipeline::generate_f16(
                &options.prompt,
                options.height,
                options.width,
                options.steps.unwrap_or(options.variant.default_steps()),
                options.guidance,
                options.seed,
                &directory,
                &weights,
                None,
                &mary::nn::backend::WgpuDevice::default(),
            );
            GeneratedImage::from_rgb(image, started.elapsed())
        }
    }
}

/// One generation request: provider-specific, typed options.
#[derive(Clone, Debug)]
pub enum Request {
    Flux(FluxOptions),
    OpenAi(super::openai::Options),
}
impl Request {
    pub fn prompt(&self) -> &str {
        match self {
            Self::Flux(options) => &options.prompt,
            Self::OpenAi(options) => &options.prompt,
        }
    }
    pub fn provider(&self) -> &'static str {
        match self {
            Self::Flux(_) => "flux",
            Self::OpenAi(_) => "openai",
        }
    }
    /// Local checks only: no credential, model or network is touched.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Flux(options) => options.validate(),
            Self::OpenAi(options) => options.validate(),
        }
    }
}

/// One answered generation, exactly as its provider produced it.
#[derive(Clone, Debug)]
pub enum Generated {
    Flux(GeneratedImage),
    OpenAi(super::openai::Generated),
}
impl Generated {
    /// The original encoded bytes (FLUX: its PNG; OpenAI: the decoded answer).
    pub fn bytes(&self) -> &Bytes {
        match self {
            Self::Flux(image) => &image.png,
            Self::OpenAi(answer) => &answer.bytes,
        }
    }
    pub fn media_type(&self) -> &'static str {
        match self {
            Self::Flux(_) => "image/png",
            Self::OpenAi(answer) => answer.media_type(),
        }
    }
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Flux(_) => "png",
            Self::OpenAi(answer) => answer.format.extension(),
        }
    }
    pub fn diagnostic(&self) -> String {
        match self {
            Self::Flux(image) => image.diagnostic(),
            Self::OpenAi(answer) => answer.diagnostic(),
        }
    }
}

/// A stored generation: the Files entity holding the bytes, the generation
/// entity describing them (both in one Files commit), and the result itself.
#[derive(Clone, Debug)]
pub struct Receipt {
    pub file: Id,
    pub generation: Id,
    pub generated: Generated,
    /// Set when the Files commit stands but the upkeep of its derived views
    /// failed: the image is stored, and readers fall back to the raw facts
    /// until maintenance catches up.
    pub warning: Option<String>,
}

/// Image generation with automatic provenance. Every successful generation
/// is published to the selected Files collection as one commit holding the
/// file and its generation record; a failed provider call writes nothing.
///
/// Remote providers use the supplied [`Storage`] as given, workspace routes
/// included: Files for the result, Secrets and the `imagine` collection for
/// the credential. Only the local FLUX path depends on model assets, whose
/// workspace routing does not exist yet, so a routed storage has no FLUX.
#[derive(Clone, Debug)]
pub struct Imagine {
    storage: Storage,
    flux: Option<ModelSources>,
    openai: super::openai::Endpoint,
    /// Where a generated image goes when it could not be stored in Files.
    unstored: Option<PathBuf>,
}
impl Imagine {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(Storage::new(pile, key))
    }
    /// Reads launcher configuration only (FLUX model locations, the OpenAI
    /// base URL); opens nothing.
    pub fn with_storage(storage: Storage) -> Self {
        let flux = storage
            .collection_routes()
            .is_none()
            .then(ModelSources::from_environment);
        Self {
            storage,
            flux,
            openai: super::openai::Endpoint::from_environment(),
            unstored: std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".cache/faculties/imagine-unstored")),
        }
    }
    /// Where images that could not be stored in Files are kept instead
    /// (default `~/.cache/faculties/imagine-unstored`).
    pub fn with_unstored_directory(mut self, directory: PathBuf) -> Self {
        self.unstored = Some(directory);
        self
    }
    /// Explicit FLUX model locations. A routed storage still has no FLUX.
    pub fn with_flux_sources(mut self, sources: ModelSources) -> Self {
        self.flux = self
            .storage
            .collection_routes()
            .is_none()
            .then_some(sources);
        self
    }
    /// Explicit OpenAI endpoint (a proxy, or a fake server in tests).
    pub fn with_openai_endpoint(mut self, endpoint: super::openai::Endpoint) -> Self {
        self.openai = endpoint;
        self
    }
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Generate, then publish the exact bytes and their provenance in one
    /// Files commit. Files WRITE is proven before the provider is called. The
    /// provider is called at most once; nothing is retried.
    ///
    /// Once the commit stands the result is a [`Receipt`], with a warning if
    /// the derived views could not be brought up to date. If the provider
    /// produced an image but it could not be committed, the exact bytes are
    /// written to the unstored directory (mode 0600) and the error names that
    /// path, or says that writing it failed too.
    pub fn generate(&self, request: &Request) -> Result<Receipt> {
        request.validate()?;
        if let Request::Flux(_) = request {
            // Both refusals come before any storage is touched.
            anyhow::ensure!(
                self.flux.is_some(),
                "imagine generate: workspace model-asset routing is unconfigured"
            );
            anyhow::ensure!(
                cfg!(feature = "imagine"),
                "FLUX generation requires a build with the `imagine` feature"
            );
        }
        let mut produced: Option<Generated> = None;
        let mut hint = request.provider().to_owned();
        let mut committed = false;
        let result = self.storage.scope(|storage| {
            let files = crate::files::Files::with_storage(storage.clone());
            files
                .preflight_write()
                .context("imagine: the result could not be stored, so nothing was generated")?;
            let requested_at = crate::clock::point_now()?;
            let nanos = crate::clock::tai_nanoseconds_now()?;
            let generated = match request {
                Request::Flux(options) => Generated::Flux(
                    Flux::new(self.flux.clone().expect("checked above")).generate(options)?,
                ),
                Request::OpenAi(options) => {
                    let key = super::auth::open_key(storage, super::auth::Provider::OpenAi)?;
                    Generated::OpenAi(super::openai::generate(&self.openai, &key, options)?)
                }
            };
            produced = Some(generated.clone());
            let name = format!(
                "imagine-{}-{nanos}.{}",
                request.provider(),
                generated.extension()
            );
            let staged = super::provenance::stage(request, &generated, requested_at, &name)
                .context("staging its Files record failed")?;
            hint = format!("{}-{:x}", request.provider(), staged.generation);
            let published = files
                .publish(staged.fragment)
                .context("committing it to Files failed")?;
            committed = true;
            let warning = match published {
                crate::files::Published::Unconfirmed { close, .. } => {
                    return Err(close.context(
                        "the Files commit was appended, but closing storage afterwards failed",
                    ));
                }
                crate::files::Published::Current(_) => None,
                crate::files::Published::Lagging { upkeep, .. } => Some(format!(
                    "the image is stored, but bringing Files' derived views up to date failed \
                     ({upkeep:#}); readers use the raw facts until maintenance catches up"
                )),
            };
            Ok(Receipt {
                file: staged.file,
                generation: staged.generation,
                generated,
                warning,
            })
        });
        match (result, produced) {
            (Ok(receipt), _) => Ok(receipt),
            (Err(error), Some(generated)) => {
                Err(self.keep_unstored(error, &generated, &hint, committed))
            }
            (Err(error), None) => Err(error),
        }
    }

    /// The image was produced but is not known to be stored: keep its exact
    /// bytes in a private local file and say where, or say that this failed.
    fn keep_unstored(
        &self,
        error: anyhow::Error,
        generated: &Generated,
        hint: &str,
        committed: bool,
    ) -> anyhow::Error {
        let state = if committed {
            "its Files commit was appended, but storage reported a failure afterwards, so its durability is not established"
        } else {
            "it was not stored in Files"
        };
        match save_unstored(self.unstored.as_deref(), hint, generated) {
            Ok(path) => error.context(format!(
                "the image was generated but {state}; its exact bytes ({} bytes, {}) were saved to {}; generation is not retried",
                generated.bytes().len(),
                generated.media_type(),
                path.display()
            )),
            Err(Unstored { file, error: write }) => {
                // Say only what is known: a created file may hold some or
                // all of the bytes; without one, no local copy exists.
                let local = match file {
                    Some(path) => format!(
                        "the local file {} may be incomplete",
                        path.display()
                    ),
                    None => "no local copy was made".to_owned(),
                };
                error.context(format!(
                    "the image was generated but {state}, and saving a local copy failed ({write:#}); {local}; generation is not retried"
                ))
            }
        }
    }

    /// Metadata only: no Secrets version is opened.
    pub fn auth_status(&self, provider: super::auth::Provider) -> Result<super::auth::AuthStatus> {
        super::auth::auth_status(&self.storage, provider)
    }

    /// Bind one exact Secrets selector for `provider`, proving it opens.
    pub fn auth_set(
        &self,
        provider: super::auth::Provider,
        selector: super::auth::Selector,
    ) -> Result<(Id, super::auth::Credential)> {
        super::auth::auth_set(&self.storage, provider, selector)
    }
}

/// Why [`save_unstored`] failed, and the file it created, if it got that far.
struct Unstored {
    file: Option<PathBuf>,
    error: anyhow::Error,
}

/// Test-only: make the next unstored write on this thread fail after it
/// created its file and wrote half the bytes.
#[cfg(test)]
pub(super) mod unstored_fault {
    use std::cell::Cell;
    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
    }
    pub(in crate::imagine) fn arm() {
        ARMED.with(|armed| armed.set(true));
    }
    pub(super) fn take() -> bool {
        ARMED.with(|armed| armed.replace(false))
    }
}

/// Write `generated`'s exact bytes to `directory/<UTC time>-<hint>.<ext>`,
/// readable by this user only, never overwriting an existing file.
fn save_unstored(
    directory: Option<&Path>,
    hint: &str,
    generated: &Generated,
) -> std::result::Result<PathBuf, Unstored> {
    use std::io::Write as _;
    let before = |error: anyhow::Error| Unstored { file: None, error };
    let directory = directory
        .context("no HOME, so there is no unstored-image directory")
        .map_err(before)?;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(directory)
        .with_context(|| format!("create {}", directory.display()))
        .map_err(before)?;
    let (year, month, day, hour, minute, second, nanos) =
        crate::clock::now().map_err(before)?.to_gregorian_utc();
    let hint: String = hint
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = directory.join(format!(
        "{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}.{:03}Z-{hint}.{}",
        nanos / 1_000_000,
        generated.extension()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&path)
        .with_context(|| format!("create {}", path.display()))
        .map_err(before)?;
    let bytes: &[u8] = generated.bytes();
    #[cfg(test)]
    if unstored_fault::take() {
        let _ = file.write_all(&bytes[..bytes.len() / 2]);
        return Err(Unstored {
            file: Some(path),
            error: anyhow::anyhow!("injected unstored write failure"),
        });
    }
    match file.write_all(bytes).and_then(|()| file.sync_all()) {
        Ok(()) => Ok(path),
        Err(error) => Err(Unstored {
            error: anyhow::Error::new(error).context(format!("write {}", path.display())),
            file: Some(path),
        }),
    }
}

pub(super) fn remember_range(raw: &str) -> Result<(hifitime::Epoch, hifitime::Epoch)> {
    use crate::memory::operations::{parse_tai_timestamp, parse_time_range};
    let range = if raw.contains("..") {
        parse_time_range(raw)?
    } else {
        let end = parse_tai_timestamp(raw)?;
        (
            end - hifitime::Duration::from_seconds(crate::memory::MOMENT_SECONDS),
            end,
        )
    };
    ensure!(
        range.1 > range.0,
        "a remembered image needs a nonzero time range"
    );
    Ok(range)
}
