//! Safe native-service occurrence vocabulary shared by publishers and Orient.
//!
//! Events use existing metadata, health and telemetry attributes, and live in
//! the publisher's existing metrics root. No descriptor or routing role is
//! inferred from this kind. Descriptions are annotations, never trusted input
//! for model-facing alerts.

use triblespace::prelude::*;

/// Minted verbatim with `trible genid` on 2026-10-10.
pub const KIND_SERVICE_EVENT: Id = triblespace::macros::id_hex!("C512D8EEC43C457F7C8D58786F59C26A");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServicePhase {
    Stop,
    Startup,
    Session,
    Cleanup,
}

impl ServicePhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Startup => "startup",
            Self::Session => "session",
            Self::Cleanup => "cleanup",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "stop" => Some(Self::Stop),
            "startup" => Some(Self::Startup),
            "session" => Some(Self::Session),
            "cleanup" => Some(Self::Cleanup),
            _ => None,
        }
    }
}

/// Allowlisted cause codes, not arbitrary local error chains or descriptions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceCode {
    ResourceLimit,
    ResourceRssLimit,
    ResourceHeadroomLimit,
    ResourceObservationFailed,
    ConfigurationUnavailable,
    SettingsChanged,
    ApplicationShutdown,
    StartupFailed,
    SessionFailed,
    WorkerPanicked,
    EndpointShutdownFailed,
    EndpointShutdownTimeout,
    StoreCloseFailed,
}

impl ServiceCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ResourceLimit => "resource_limit",
            Self::ResourceRssLimit => "resource_rss_limit",
            Self::ResourceHeadroomLimit => "resource_headroom_limit",
            Self::ResourceObservationFailed => "resource_observation_failed",
            Self::ConfigurationUnavailable => "configuration_unavailable",
            Self::SettingsChanged => "settings_changed",
            Self::ApplicationShutdown => "application_shutdown",
            Self::StartupFailed => "startup_failed",
            Self::SessionFailed => "session_failed",
            Self::WorkerPanicked => "worker_panicked",
            Self::EndpointShutdownFailed => "endpoint_shutdown_failed",
            Self::EndpointShutdownTimeout => "endpoint_shutdown_timeout",
            Self::StoreCloseFailed => "store_close_failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "resource_limit" => Some(Self::ResourceLimit),
            "resource_rss_limit" => Some(Self::ResourceRssLimit),
            "resource_headroom_limit" => Some(Self::ResourceHeadroomLimit),
            "resource_observation_failed" => Some(Self::ResourceObservationFailed),
            "configuration_unavailable" => Some(Self::ConfigurationUnavailable),
            "settings_changed" => Some(Self::SettingsChanged),
            "application_shutdown" => Some(Self::ApplicationShutdown),
            "startup_failed" => Some(Self::StartupFailed),
            "session_failed" => Some(Self::SessionFailed),
            "worker_panicked" => Some(Self::WorkerPanicked),
            "endpoint_shutdown_failed" => Some(Self::EndpointShutdownFailed),
            "endpoint_shutdown_timeout" => Some(Self::EndpointShutdownTimeout),
            "store_close_failed" => Some(Self::StoreCloseFailed),
            _ => None,
        }
    }

    /// An intentional settings change or app shutdown is not a saved error.
    pub const fn requires_attention(self) -> bool {
        !matches!(self, Self::SettingsChanged | Self::ApplicationShutdown)
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::ResourceLimit => "The service resource guard refused continued operation.",
            Self::ResourceRssLimit => "Process RSS reached the 48 GiB service limit.",
            Self::ResourceHeadroomLimit => "Kernel memory headroom fell below 20%.",
            Self::ResourceObservationFailed => {
                "The service resource guard could not obtain a valid memory observation."
            }
            Self::ConfigurationUnavailable => "Node configuration became unavailable.",
            Self::SettingsChanged => "Service settings changed.",
            Self::ApplicationShutdown => "The application requested service shutdown.",
            Self::StartupFailed => "Node services failed before readiness.",
            Self::SessionFailed => "The node service session failed.",
            Self::WorkerPanicked => "The node service worker panicked.",
            Self::EndpointShutdownFailed => "The owned native endpoint failed to shut down.",
            Self::EndpointShutdownTimeout => {
                "The owned native endpoint exceeded its five-second shutdown deadline."
            }
            Self::StoreCloseFailed => "The owned node store handle failed to close.",
        }
    }
}
