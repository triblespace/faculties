//! CLI-only notebook harness integration. GORBIE's macro remains in each thin
//! binary so its per-binary native/window/headless/web-export UX is preserved.
//! Help and version are handled before starting any window or GPU context.

use std::io::Write;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
struct WorkspaceSelection {
    #[command(flatten)]
    workspace: crate::cli_workspace::WorkspaceArgs,
}

/// The macro-owned notebook callback cannot receive a fallible, frozen
/// launcher binding yet. Refuse selected mode before starting its renderer;
/// never accept an environment selector and then open legacy collection roots.
fn validate_workspace_support(args: &[String]) -> Result<()> {
    let mut selection = vec!["viewer".to_owned()];
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--" => break,
            "--workspace" => {
                selection.push(arg.clone());
                if let Some(value) = args.next() {
                    selection.push(value.clone());
                }
            }
            "--pile" | "--out-dir" | "--export-dir" | "--scale" | "--pixels-per-point"
            | "--headless-wait-ms" | "--theme" => {
                args.next();
            }
            _ if arg.starts_with("--workspace=") => selection.push(arg.clone()),
            _ => {}
        }
    }
    let parsed = WorkspaceSelection::try_parse_from(selection)?;
    anyhow::ensure!(
        parsed.workspace.workspace.is_none(),
        "standalone Viewer/capture notebooks do not yet support workspace-selected startup; omit --workspace and unset FACULTIES_WORKSPACE to use the existing standalone mode. Erlkonig's explicitly routed Viewer integration is unchanged"
    );
    Ok(())
}

pub fn information(binary: &str, args: &[String]) -> Option<String> {
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        return Some(format!(
            "{} {} ({})\n",
            binary,
            env!("CARGO_PKG_VERSION"),
            env!("FACULTIES_GIT_VERSION")
        ));
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Some(format!(
            "Usage: {binary} [PILE] [OPTIONS]\n\n\
             Open the shared faculty notebook; PILE defaults to $PILE or ./self.pile.\n\n\
             --pile PATH              Override positional/environment pile\n\
             --workspace REVISION     Not supported by standalone notebooks; FACULTIES_WORKSPACE is also refused\n\
             --headless               Write PNG card captures instead of opening a window\n\
             --out-dir PATH           Capture directory (default gorbie_capture)\n\
             --scale NUMBER           Pixels per point (default 2)\n\
             --headless-wait-ms MS     Settle timeout per layout pass (default 2000)\n\
             --export                 Explicit web export (requires web-export feature)\n\
             --export-dir PATH        Explicit web export destination\n\
             -h, --help               Show this help without opening the pile or GPU\n\
             -V, --version            Show version without opening the pile or GPU\n"
        ));
    }
    None
}

/// The only global argument/terminal boundary; native operations do not call
/// this. The supplied notebook wrapper owns GORBIE's existing harness syntax.
pub fn run(binary: &str, notebook: impl FnOnce()) -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if let Some(info) = information(binary, &args) {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(info.as_bytes())?;
        stdout.flush()?;
    } else {
        validate_workspace_support(&args)?;
        notebook();
    }
    Ok(())
}

#[cfg(feature = "widgets")]
pub fn configuration_from_env() -> super::Viewer {
    super::Viewer::new(
        crate::widgets::resolve_pile_path(std::env::args().skip(1), std::env::var("PILE").ok()),
        None,
    )
}

#[cfg(test)]
mod workspace_tests {
    use super::*;

    #[test]
    fn explicit_selection_is_refused_for_every_standalone_notebook() {
        let revision = format!("{:X}", triblespace::prelude::genid().id);
        for args in [
            vec!["--workspace".into(), revision.clone()],
            vec!["--headless".into(), format!("--workspace={revision}")],
        ] {
            let error = validate_workspace_support(&args).unwrap_err();
            assert!(error
                .to_string()
                .contains("do not yet support workspace-selected"));
        }
    }

    #[test]
    fn selection_limitation_is_documented_in_storage_free_help() {
        let help = information(
            "files-capture",
            &["--help".into(), "--workspace=invalid".into()],
        )
        .unwrap();
        assert!(help.contains("FACULTIES_WORKSPACE is also refused"));
    }

    #[test]
    fn inherited_selection_is_refused_before_notebook_launch() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "viewer::cli::workspace_tests::inherited_selection_child",
                "--nocapture",
            ])
            .env("FACULTIES_VIEWER_WORKSPACE_TEST", "1")
            .env(
                "FACULTIES_WORKSPACE",
                format!("{:X}", triblespace::prelude::genid().id),
            )
            .env_remove("PILE")
            .env_remove("TRIBLESPACE_KEY")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[test]
    fn inherited_selection_child() {
        if std::env::var_os("FACULTIES_VIEWER_WORKSPACE_TEST").is_none() {
            return;
        }
        let error = run("viewer", || panic!("renderer must not start")).unwrap_err();
        assert!(error
            .to_string()
            .contains("do not yet support workspace-selected"));
        let cli = crate::duplex::cli::Cli::try_parse_from(["duplex", "devices"]).unwrap();
        let error = crate::duplex::cli::execute(cli, &mut crate::out::Out::new(&mut |_| Ok(())))
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("does not yet support workspace-selected"));
        let cli = crate::body::cli::Cli::try_parse_from(["body", "pose"]).unwrap();
        let error =
            crate::body::cli::execute(cli, &mut crate::out::Out::new(&mut |_| Ok(()))).unwrap_err();
        assert!(error.to_string().contains("hardware-only commands"));
    }
}
