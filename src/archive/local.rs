//! Filesystem adapter dispatch shared by explicit import and local session ingestion.
use super::{ImportSource, ImportSummary};
use crate::blockdag::Identity;
use crate::{
    archive_agy, archive_chatgpt, archive_chatgpt_library, archive_claude_code, archive_claude_web,
    archive_codex, archive_codex_cloud, archive_copilot, archive_gemini,
};
use anyhow::Result;
use std::path::Path;
use std::time::SystemTime;
use triblespace::core::trible::Fragment;

pub(super) fn project_path(
    path: &Path,
    source: ImportSource,
    finished_before: Option<SystemTime>,
    mut emit: impl FnMut(Fragment, Identity, &Path) -> Result<()>,
) -> Result<ImportSummary> {
    match source {
        ImportSource::Agy => archive_agy::project_path_finished(path, finished_before, |p| {
            emit(p.fragment, p.identity, &p.source_path)
        })
        .map(ImportSummary::Agy),
        ImportSource::ChatGpt => {
            archive_chatgpt::project_path(path, |p| emit(p.fragment, p.identity, &p.source_path))
                .map(ImportSummary::ChatGpt)
        }
        ImportSource::ChatGptLibrary => archive_chatgpt_library::project_path(path, |p| {
            emit(p.fragment, p.identity, &p.source_path)
        })
        .map(ImportSummary::ChatGptLibrary),
        ImportSource::ClaudeCode => {
            archive_claude_code::project_path_finished(path, finished_before, |p| {
                emit(p.fragment, p.identity, &p.source_path)
            })
            .map(ImportSummary::ClaudeCode)
        }
        ImportSource::ClaudeWeb => {
            archive_claude_web::project_path(path, |p| emit(p.fragment, p.identity, &p.source_path))
                .map(ImportSummary::ClaudeWeb)
        }
        ImportSource::Codex => archive_codex::project_path_finished(path, finished_before, |p| {
            emit(p.fragment, p.identity, &p.source_path)
        })
        .map(ImportSummary::Codex),
        ImportSource::CodexCloud => archive_codex_cloud::project_path(path, |p| {
            emit(p.fragment, p.identity, &p.source_path)
        })
        .map(ImportSummary::CodexCloud),
        ImportSource::Copilot => {
            archive_copilot::project_path(path, |p| emit(p.fragment, p.identity, &p.source_path))
                .map(ImportSummary::Copilot)
        }
        ImportSource::Gemini => {
            archive_gemini::project_path(path, |p| emit(p.fragment, p.identity, &p.source_path))
                .map(ImportSummary::Gemini)
        }
    }
}
