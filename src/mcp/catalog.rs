//! The aggregate native faculty catalogue, independent of its transport.
//!
//! A launcher supplies trusted configuration explicitly. Construction and tool
//! discovery do not read environment variables, open a pile or signing key,
//! load a model, or contact an external service.

use anyhow::{bail, Result};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::Faculty;
use crate::storage::Storage;
use crate::{
    alarm, archive, atlas, body, bootstrap, care, code, cognition, compass, decide, discord,
    duplex, files, gauge, habits, headspace, hear, home, imagine, linkedin, mail, memory, message,
    orient, patience, planner, posture, quota, reason, relations, search, secrets, teams, triage,
    viewer, voice, web, wiki, world,
};

/// Trusted launcher configuration, never supplied as MCP tool arguments.
/// Credentials are deliberately not included in a derived debug formatter.
pub struct Config {
    pub pile: PathBuf,
    pub key: Option<PathBuf>,
    pub linkedin_token: Option<String>,
    pub duplex_session: Option<PathBuf>,
    pub hear: Option<hear::ModelConfig>,
    /// World server base URL; the world faculty's default when unset.
    pub world_url: Option<String>,
    /// Session file shared with the `world` CLI; in memory when unset.
    pub world_session: Option<PathBuf>,
    /// Where alarm rungs go; every target unset when not configured.
    pub alarm: alarm::rungs::Settings,
}

impl Config {
    /// Configure one pile, leaving optional credentials and model assets unset.
    /// Neither defaults nor construction consult the process environment.
    pub fn new(pile: impl Into<PathBuf>) -> Self {
        Self {
            pile: pile.into(),
            key: None,
            linkedin_token: None,
            duplex_session: None,
            hear: None,
            world_url: None,
            world_session: None,
            alarm: alarm::rungs::Settings::default(),
        }
    }
}

/// Owned native adapters in the aggregate's stable registration order.
/// Protocol/session state belongs to a transport's [`super::Server`], not here.
pub struct Catalog {
    faculties: Vec<Box<dyn Faculty + Send + Sync>>,
    storage: Storage,
    orient: Arc<orient::mcp::Orient>,
}

impl Catalog {
    pub fn new(config: Config) -> Self {
        let storage = Storage::shared(config.pile.clone(), config.key.clone());
        Self::with_storage(config, storage)
    }

    /// Use a launcher-owned shared storage context, including its explicit
    /// network routes. The supplied owner is also the one closed at shutdown.
    pub fn with_storage(config: Config, storage: Storage) -> Self {
        let orient = orient::mcp::Orient::with_storage(storage.clone());
        Self::with_orient(config, storage, orient)
    }

    /// Native model delivery accepts complete results after publication, not
    /// at the generic adapter's output buffer. Select non-consuming Orient
    /// overview/wake/poll schemas on the same registered adapter; next retains
    /// its explicit prepared acceptance. Baseline remains explicit discard.
    pub fn with_native_storage(config: Config, storage: Storage) -> Self {
        let orient = orient::mcp::Orient::with_native_storage(storage.clone());
        Self::with_orient(config, storage, orient)
    }

    fn with_orient(config: Config, storage: Storage, orient: orient::mcp::Orient) -> Self {
        let Config {
            pile: _,
            key: _,
            linkedin_token,
            duplex_session,
            hear: hear_config,
            world_url,
            world_session,
            alarm: alarm_settings,
        } = config;
        let discord = discord::mcp::Discord::with_storage(storage.clone());
        let linkedin = linkedin::mcp::LinkedIn::with_storage(storage.clone());
        let linkedin = match linkedin_token {
            Some(token) => linkedin.with_token(token),
            None => linkedin,
        };
        let orient = Arc::new(orient);
        let faculties: Vec<Box<dyn Faculty + Send + Sync>> = vec![
            Box::new(alarm::mcp::Alarm::with_settings(
                storage.clone(),
                alarm_settings,
            )),
            Box::new(archive::mcp::Archive::with_storage(storage.clone())),
            Box::new(atlas::mcp::Atlas::with_storage(storage.clone())),
            Box::new(body::mcp::Body::with_storage(storage.clone())),
            Box::new(bootstrap::mcp::Bootstrap::with_storage(storage.clone())),
            Box::new(care::mcp::Care::with_storage(storage.clone())),
            Box::new(code::mcp::Code::with_storage(storage.clone())),
            Box::new(cognition::mcp::Cognition::with_storage(storage.clone())),
            Box::new(compass::mcp::Compass::with_storage(storage.clone())),
            Box::new(decide::mcp::Decide::with_storage(storage.clone())),
            Box::new(discord),
            Box::new(duplex::mcp::Duplex::new(duplex_session)),
            Box::new(files::mcp::Files::with_storage(storage.clone())),
            Box::new(gauge::mcp::Gauge::with_storage(storage.clone())),
            Box::new(habits::mcp::Habits::with_storage(storage.clone())),
            Box::new(headspace::mcp::Headspace::with_storage(storage.clone())),
            Box::new(hear::mcp::Hear::with_storage(storage.clone(), hear_config)),
            Box::new(home::mcp::Home::with_storage(storage.clone())),
            Box::new(imagine::mcp::Imagine::with_storage(storage.clone())),
            Box::new(linkedin),
            Box::new(mail::mcp::Mail::with_storage(storage.clone())),
            Box::new(memory::mcp::Memory::with_storage(storage.clone())),
            Box::new(message::mcp::Message::with_storage(storage.clone())),
            Box::new(orient.clone()),
            Box::new(patience::mcp::Patience::with_storage(storage.clone())),
            Box::new(planner::mcp::Planner::with_storage(storage.clone())),
            Box::new(posture::mcp::Posture::with_storage(storage.clone())),
            Box::new(quota::mcp::Quota::with_storage(storage.clone())),
            Box::new(reason::mcp::Reason::with_storage(storage.clone())),
            Box::new(relations::mcp::Relations::with_storage(storage.clone())),
            Box::new(search::mcp::Search::with_storage(storage.clone())),
            Box::new(secrets::mcp::Secrets::with_storage(storage.clone())),
            Box::new(teams::mcp::Teams::with_storage(storage.clone())),
            Box::new(triage::mcp::Triage::with_storage(storage.clone())),
            Box::new(viewer::mcp::Viewer::with_storage(storage.clone())),
            Box::new(voice::mcp::Voice::with_storage(storage.clone())),
            Box::new(web::mcp::Web::with_storage(storage.clone())),
            Box::new(wiki::mcp::Wiki::with_storage(storage.clone())),
            Box::new(world::mcp::WorldTools::new(world_url, world_session)),
        ];
        Self {
            faculties,
            storage,
            orient,
        }
    }

    /// Flush and close the application-owned store after transport shutdown.
    /// Protocol session expiry does not close this shared owner.
    pub fn close(&self) -> Result<()> {
        self.storage.close()
    }

    /// Close storage without losing a transport's error, if it also failed.
    pub fn finish<T>(&self, result: Result<T>) -> Result<T> {
        self.storage.finish(result)
    }

    /// Project the actual registered schemas before preparing a model prompt.
    /// No second registration table or transport initialization is needed.
    pub fn tools(&self) -> impl Iterator<Item = (&super::Tool, &str)> {
        self.faculties.iter().flat_map(|faculty| {
            faculty
                .tools()
                .iter()
                .map(move |tool| (tool, faculty.input_schema(tool)))
        })
    }

    /// Invoke the registered adapter directly on an application-owned worker.
    /// Generic operations retain their usual side-effect/Out acceptance rules.
    pub fn invoke(
        &self,
        name: &str,
        arguments: super::Bytes,
        limits: super::Limits,
        cancelled: &AtomicBool,
    ) -> Result<super::NativeResult> {
        if name == "orient_next" {
            bail!("native orient_next requires prepare_next and acknowledgment after publication");
        }
        super::native_argument_budget(&arguments, limits)?;
        let faculty = self
            .faculties
            .iter()
            .find(|faculty| faculty.tools().iter().any(|tool| tool.name == name))
            .ok_or_else(|| anyhow::anyhow!("unknown registered faculty tool {name:?}"))?;
        super::invoke_native(
            faculty.as_ref(),
            name,
            arguments,
            limits.response_bytes,
            cancelled,
        )
    }

    /// The registered Orient adapter, with deferred native acceptance. Its
    /// continuation shares this Catalog's store; close only after workers join
    /// and every published result has either acknowledged or reported failure.
    pub fn prepare_next(&self, arguments: super::Bytes) -> Result<orient::Next> {
        super::native_argument_budget(&arguments, super::Limits::default())?;
        self.orient.prepare_next(arguments)
    }

    /// Borrow the same adapters for an HTTP listener.
    pub fn registrations(&self) -> Vec<&dyn Faculty> {
        self.faculties
            .iter()
            .map(|faculty| faculty.as_ref() as &dyn Faculty)
            .collect()
    }

    /// Borrow the adapters for the stdio transport, which runs calls
    /// concurrently and so needs them shareable across threads.
    pub fn sync_registrations(&self) -> Vec<&(dyn Faculty + Sync)> {
        self.faculties
            .iter()
            .map(|faculty| faculty.as_ref() as &(dyn Faculty + Sync))
            .collect()
    }
}
