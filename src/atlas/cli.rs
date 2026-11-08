//! Atlas's CLI grammar and text projection. Rust callers use [`super::Store`]
//! directly; MCP has its own typed argument boundary in [`super::mcp`].

use anyhow::{bail, Result};

use super::{render, Store};
use crate::out::Out;
use crate::spec::{Invocation, Param, Spec, Verb};

pub static SPEC: Spec = Spec {
    name: "atlas",
    about: "Schema metadata inspection faculty",
    version: Some(crate::GIT_VERSION),
    shared: &[
        Param::caller("pile", "Path to the pile file to use")
            .path()
            .ambient()
            .env("PILE"),
        Param::caller(
            "key",
            "Existing durable signing-key file. Reads and writes never create it.",
        )
        .path()
        .ambient()
        .optional()
        .env("TRIBLESPACE_KEY"),
        Param::caller(
            "workspace",
            "Exact workspace revision ID; omit for legacy collection routing",
        )
        .ambient()
        .optional()
        .env("FACULTIES_WORKSPACE"),
    ],
    verbs: &[
        Verb {
            name: "list",
            about: "List entities that have metadata::name entries",
            params: &[],
        },
        Verb {
            name: "show",
            about: "Show metadata for a single id prefix",
            params: &[Param::caller("id", "Entity id or unique prefix").positional()],
        },
    ],
};

/// Execute a parsed CLI request. Output routing belongs to `crate::cli`.
pub fn execute(invocation: &Invocation, output: &mut Out<'_>) -> Result<()> {
    let workspace = crate::cli_workspace::WorkspaceArgs {
        workspace: invocation
            .get("workspace")
            .map(crate::cli_workspace::parse_revision)
            .transpose()
            .map_err(anyhow::Error::msg)?,
    };
    let store = Store::with_storage(workspace.storage(
        invocation.require_path("pile")?.to_owned(),
        invocation.path("key").map(std::path::Path::to_owned),
    )?);
    let result = (|| {
        match invocation.verb().name {
            "list" => {
                for row in store.list()? {
                    output.line(render::list_line(&row))?;
                }
            }
            "show" => {
                for line in render::show_lines(&store.show(invocation.require("id")?)?) {
                    output.line(line)?;
                }
            }
            other => bail!("Atlas CLI has no verb {other:?}"),
        }
        Ok(())
    })();
    store.finish(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_is_global_and_no_command_remains_help() {
        use crate::spec::CliRequest;
        let id = format!("{:X}", triblespace::prelude::genid().id);
        for args in [
            vec![
                "atlas",
                "--pile",
                "/absent.pile",
                "--workspace",
                &id,
                "list",
            ],
            vec![
                "atlas",
                "--pile",
                "/absent.pile",
                "list",
                "--workspace",
                &id,
            ],
        ] {
            let CliRequest::Invoke(invocation) = SPEC.lower_cli_from(args).unwrap() else {
                panic!("expected invocation");
            };
            assert_eq!(invocation.get("workspace"), Some(id.as_str()));
        }
        assert!(matches!(
            SPEC.lower_cli_from(["atlas", "--workspace", &id]).unwrap(),
            CliRequest::Help(_)
        ));
    }

    #[test]
    fn invalid_workspace_fails_before_storage() {
        let crate::spec::CliRequest::Invoke(invocation) = SPEC
            .lower_cli_from([
                "atlas",
                "--pile",
                "/absent.pile",
                "--workspace",
                "not-an-id",
                "list",
            ])
            .unwrap()
        else {
            panic!("expected invocation");
        };
        let error = execute(&invocation, &mut Out::new(&mut |_| panic!("no output"))).unwrap_err();
        assert!(error.to_string().contains("workspace"), "{error:#}");
    }

    #[test]
    fn cli_retains_the_atlas_surface() {
        let command = SPEC.to_clap();
        assert_eq!(command.get_version(), Some(crate::GIT_VERSION));
        assert_eq!(
            command
                .get_subcommands()
                .map(|command| command.get_name())
                .collect::<Vec<_>>(),
            ["list", "show"]
        );
        for forbidden in ["scope", "branch", "branch_id", "head", "repair"] {
            assert!(!command
                .get_arguments()
                .any(|argument| argument.get_id() == forbidden));
        }
        assert!(command
            .get_arguments()
            .any(|argument| argument.get_id() == "key"));
    }
}
