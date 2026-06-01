//! External language-tool installer planning.
//!
//! `cargo`, `go`, `uv`, and `npm` all share the same update shape: run one
//! tool-specific command, expect a binary under a managed install root, then
//! link that binary into `SHDEPS_BIN_DIR`. The command details differ enough
//! that keeping them behind one planner is safer than scattering string
//! construction through the update orchestrator.

use std::path::{Path, PathBuf};

use crate::method;
use crate::pkg::CommandSpec;

/// Planned external installer invocation and expected output path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Tool that must exist on `PATH` before the install can run.
    pub tool: String,
    /// Command to run for a normal install.
    pub command: CommandSpec,
    /// Optional argument appended when reinstall mode is active.
    pub force_arg: Option<String>,
    /// Managed install root for the dependency.
    pub install_root: PathBuf,
    /// Expected binary path produced by the install command.
    pub bin_path: PathBuf,
}

impl Plan {
    /// Returns the command adjusted for reinstall mode.
    #[must_use]
    pub fn command_for(&self, reinstall: bool) -> CommandSpec {
        let mut command = self.command.clone();
        if reinstall {
            if let Some(force_arg) = &self.force_arg {
                command.args.push(force_arg.clone());
            }
        }
        command
    }
}

/// Builds the external install plan for a supported method.
#[must_use]
pub fn plan(method: &str, name: &str, cmd: &str, install_dir: &Path) -> Option<Plan> {
    let install_root = install_dir.join(name);
    let bin_path = install_root.join("bin").join(cmd);
    let tool = method::external_tool(method)?;

    let (command, force_arg) = match method {
        method::CARGO => (
            CommandSpec {
                program: "cargo".to_owned(),
                args: vec![
                    "install".to_owned(),
                    "--locked".to_owned(),
                    "--root".to_owned(),
                    install_root.display().to_string(),
                    name.to_owned(),
                ],
            },
            Some("--force".to_owned()),
        ),
        method::GO => (
            CommandSpec {
                program: "env".to_owned(),
                args: vec![
                    format!("GOBIN={}", install_root.join("bin").display()),
                    "go".to_owned(),
                    "install".to_owned(),
                    format!("{name}@latest"),
                ],
            },
            None,
        ),
        method::UV => (
            CommandSpec {
                program: "env".to_owned(),
                args: vec![
                    format!("UV_TOOL_DIR={}", install_root.join("tools").display()),
                    format!("UV_TOOL_BIN_DIR={}", install_root.join("bin").display()),
                    "uv".to_owned(),
                    "tool".to_owned(),
                    "install".to_owned(),
                    name.to_owned(),
                ],
            },
            Some("--force".to_owned()),
        ),
        method::NPM => (
            CommandSpec {
                program: "npm".to_owned(),
                args: vec![
                    "install".to_owned(),
                    "-g".to_owned(),
                    "--prefix".to_owned(),
                    install_root.display().to_string(),
                    name.to_owned(),
                ],
            },
            None,
        ),
        _ => return None,
    };

    Some(Plan {
        tool: tool.to_owned(),
        command,
        force_arg,
        install_root,
        bin_path,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::plan;
    use crate::pkg::CommandSpec;

    #[test]
    fn cargo_plan_uses_locked_root_install_and_force_reinstall() {
        let root = PathBuf::from("/tmp/share");
        let plan = plan("cargo", "ripgrep", "rg", &root).unwrap();

        assert_eq!(plan.tool, "cargo");
        assert_eq!(plan.install_root, root.join("ripgrep"));
        assert_eq!(plan.bin_path, root.join("ripgrep/bin/rg"));
        assert_eq!(
            plan.command,
            cmd(
                "cargo",
                [
                    "install",
                    "--locked",
                    "--root",
                    "/tmp/share/ripgrep",
                    "ripgrep"
                ]
            )
        );
        assert_eq!(
            plan.command_for(true),
            cmd(
                "cargo",
                [
                    "install",
                    "--locked",
                    "--root",
                    "/tmp/share/ripgrep",
                    "ripgrep",
                    "--force",
                ]
            )
        );
    }

    #[test]
    fn go_plan_sets_gobin_and_latest_module() {
        let root = PathBuf::from("/tmp/share");
        let plan = plan("go", "github.com/junegunn/fzf", "fzf", &root).unwrap();

        assert_eq!(plan.tool, "go");
        assert_eq!(plan.install_root, root.join("github.com/junegunn/fzf"));
        assert_eq!(
            plan.command,
            cmd(
                "env",
                [
                    "GOBIN=/tmp/share/github.com/junegunn/fzf/bin",
                    "go",
                    "install",
                    "github.com/junegunn/fzf@latest",
                ]
            )
        );
        assert_eq!(plan.command_for(true), plan.command);
    }

    #[test]
    fn uv_plan_sets_tool_dirs_and_force_reinstall() {
        let root = PathBuf::from("/tmp/share");
        let plan = plan("uv", "ruff", "ruff", &root).unwrap();

        assert_eq!(plan.tool, "uv");
        assert_eq!(
            plan.command,
            cmd(
                "env",
                [
                    "UV_TOOL_DIR=/tmp/share/ruff/tools",
                    "UV_TOOL_BIN_DIR=/tmp/share/ruff/bin",
                    "uv",
                    "tool",
                    "install",
                    "ruff",
                ]
            )
        );
        assert_eq!(
            plan.command_for(true),
            cmd(
                "env",
                [
                    "UV_TOOL_DIR=/tmp/share/ruff/tools",
                    "UV_TOOL_BIN_DIR=/tmp/share/ruff/bin",
                    "uv",
                    "tool",
                    "install",
                    "ruff",
                    "--force",
                ]
            )
        );
    }

    #[test]
    fn npm_plan_uses_global_prefix_without_force_arg() {
        let root = PathBuf::from("/tmp/share");
        let plan = plan("npm", "prettier", "prettier", &root).unwrap();

        assert_eq!(plan.tool, "npm");
        assert_eq!(
            plan.command,
            cmd(
                "npm",
                [
                    "install",
                    "-g",
                    "--prefix",
                    "/tmp/share/prettier",
                    "prettier"
                ]
            )
        );
        assert_eq!(plan.command_for(true), plan.command);
    }

    #[test]
    fn unknown_method_has_no_external_plan() {
        assert!(plan("pkg", "jq", "jq", &PathBuf::from("/tmp/share")).is_none());
    }

    fn cmd(program: &str, args: impl IntoIterator<Item = impl Into<String>>) -> CommandSpec {
        CommandSpec {
            program: program.to_owned(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}
