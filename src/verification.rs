use crate::command;
use serde::Serialize;
use serde_json::Value;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_VERIFY_TIMEOUT_MS: u64 = 120_000;
pub const MAX_VERIFY_TIMEOUT_MS: u64 = 600_000;
const MAX_SUMMARY_LINES: usize = 18;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationCommand {
    pub ecosystem: String,
    pub command: String,
    pub executable: String,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationResult {
    pub ecosystem: String,
    pub command: String,
    pub status: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub elapsed_ms: u64,
    pub summary: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyProjectOutput {
    pub success: bool,
    pub status: String,
    pub commands: Vec<VerificationResult>,
    pub skipped: Vec<String>,
}

impl VerifyProjectOutput {
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("status: {}\n", self.status));
        out.push_str(&format!("success: {}\n", self.success));
        out.push_str(&format!("commands: {}\n", self.commands.len()));
        if !self.skipped.is_empty() {
            out.push_str("\nskipped:\n");
            for item in &self.skipped {
                out.push_str("- ");
                out.push_str(item);
                out.push('\n');
            }
        }
        for result in &self.commands {
            out.push_str(&format!(
                "\n## {} - {}\nstatus: {}\nexit_code: {}\nelapsed_ms: {}\n",
                result.ecosystem,
                result.command,
                result.status,
                result
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "none".into()),
                result.elapsed_ms,
            ));
            for line in &result.summary {
                out.push_str("- ");
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }
}

fn workspace_root_path(workspace_root: &str) -> Result<PathBuf, String> {
    Path::new(workspace_root)
        .canonicalize()
        .map(command::normalize_windows_verbatim_path)
        .map_err(|e| e.to_string())
}

fn executable_available(name: &str) -> bool {
    let path_exts = if cfg!(windows) {
        env::var_os("PATHEXT")
            .map(|value| {
                value
                    .to_string_lossy()
                    .split(';')
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![".EXE".into(), ".BAT".into(), ".CMD".into()])
    } else {
        Vec::new()
    };
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    for dir in env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return true;
        }
        if cfg!(windows) && Path::new(name).extension().is_none() {
            for ext in &path_exts {
                if dir.join(format!("{name}{ext}")).is_file() {
                    return true;
                }
            }
        }
    }
    false
}

fn has_python_files(root: &Path) -> bool {
    fs::read_dir(root)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .any(|entry| {
            entry
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext == "py")
        })
}

fn package_json(root: &Path) -> Option<Value> {
    let text = fs::read_to_string(root.join("package.json")).ok()?;
    serde_json::from_str::<Value>(&text).ok()
}

fn package_script_exists(root: &Path, script: &str) -> bool {
    package_json(root)
        .and_then(|value| value.get("scripts").cloned())
        .and_then(|scripts| scripts.as_object().cloned())
        .is_some_and(|scripts| scripts.contains_key(script))
}

fn node_package_manager(root: &Path) -> &'static str {
    [
        ("pnpm-lock.yaml", "pnpm"),
        ("yarn.lock", "yarn"),
        ("bun.lock", "bun"),
        ("bun.lockb", "bun"),
        ("package-lock.json", "npm"),
    ]
    .into_iter()
    .find_map(|(lockfile, manager)| root.join(lockfile).is_file().then_some(manager))
    .unwrap_or("npm")
}

fn node_script_command(manager: &str, script: &str) -> VerificationCommand {
    let command = match manager {
        "npm" | "pnpm" | "bun" => format!("{manager} run {script}"),
        "yarn" => format!("yarn {script}"),
        _ => format!("{manager} run {script}"),
    };
    let args = match manager {
        "npm" | "pnpm" | "bun" => vec!["run".into(), script.into()],
        "yarn" => vec![script.into()],
        _ => vec!["run".into(), script.into()],
    };
    VerificationCommand {
        ecosystem: "Node".into(),
        command,
        executable: manager.into(),
        args,
    }
}

fn pyproject_text(root: &Path) -> String {
    fs::read_to_string(root.join("pyproject.toml")).unwrap_or_default()
}

fn python_project_detected(root: &Path) -> bool {
    root.join("pyproject.toml").is_file()
        || root.join("setup.py").is_file()
        || root.join("requirements.txt").is_file()
        || has_python_files(root)
}

fn pytest_configured(root: &Path) -> bool {
    root.join("pytest.ini").is_file() || pyproject_text(root).contains("[tool.pytest")
}

fn ruff_configured(root: &Path) -> bool {
    root.join("ruff.toml").is_file() || pyproject_text(root).contains("[tool.ruff")
}

fn mypy_configured(root: &Path) -> bool {
    root.join("mypy.ini").is_file() || pyproject_text(root).contains("[tool.mypy")
}

pub fn verification_plan(
    workspace_root: &str,
) -> Result<(Vec<VerificationCommand>, Vec<String>), String> {
    let root = workspace_root_path(workspace_root)?;
    let mut commands = Vec::new();
    let mut skipped = Vec::new();

    if root.join("Cargo.toml").is_file() {
        commands.extend([
            VerificationCommand {
                ecosystem: "Rust".into(),
                command: "cargo fmt --check".into(),
                executable: "cargo".into(),
                args: vec!["fmt".into(), "--check".into()],
            },
            VerificationCommand {
                ecosystem: "Rust".into(),
                command: "cargo test".into(),
                executable: "cargo".into(),
                args: vec!["test".into()],
            },
            VerificationCommand {
                ecosystem: "Rust".into(),
                command: "cargo build".into(),
                executable: "cargo".into(),
                args: vec!["build".into()],
            },
        ]);
    } else {
        skipped.push("Rust: Cargo.toml not found".into());
    }

    if root.join("package.json").is_file() {
        let manager = node_package_manager(&root);
        for script in ["test", "build", "lint"] {
            if package_script_exists(&root, script) {
                commands.push(node_script_command(manager, script));
            } else {
                skipped.push(format!("Node: package.json has no `{script}` script"));
            }
        }
    } else {
        skipped.push("Node: package.json not found".into());
    }

    if python_project_detected(&root) {
        if pytest_configured(&root) {
            commands.push(VerificationCommand {
                ecosystem: "Python".into(),
                command: "pytest".into(),
                executable: "pytest".into(),
                args: Vec::new(),
            });
        } else {
            skipped.push("Python: pytest not configured".into());
        }
        if ruff_configured(&root) {
            commands.push(VerificationCommand {
                ecosystem: "Python".into(),
                command: "ruff check .".into(),
                executable: "ruff".into(),
                args: vec!["check".into(), ".".into()],
            });
        } else {
            skipped.push("Python: ruff not configured".into());
        }
        if mypy_configured(&root) {
            commands.push(VerificationCommand {
                ecosystem: "Python".into(),
                command: "mypy .".into(),
                executable: "mypy".into(),
                args: vec![".".into()],
            });
        } else {
            skipped.push("Python: mypy not configured".into());
        }
    } else {
        skipped.push("Python: no Python project files found".into());
    }

    Ok((commands, skipped))
}

fn push_unique(lines: &mut Vec<String>, line: &str) {
    let trimmed = line.trim();
    if !trimmed.is_empty() && !lines.iter().any(|existing| existing == trimmed) {
        lines.push(trimmed.to_string());
    }
}

fn summary_lines(result: &command::CommandResult) -> Vec<String> {
    let mut lines = Vec::new();

    for line in result.stderr.lines().chain(result.stdout.lines()) {
        let lower = line.to_ascii_lowercase();
        if lower.contains("error")
            || lower.contains("failed")
            || lower.contains("failure")
            || lower.contains("panic")
            || lower.contains("warning")
            || lower.contains("test result:")
        {
            push_unique(&mut lines, line);
        }
    }

    if lines.len() < MAX_SUMMARY_LINES {
        let tail = result
            .stderr
            .lines()
            .chain(result.stdout.lines())
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        for line in tail.into_iter().rev().take(8).collect::<Vec<_>>().into_iter().rev() {
            push_unique(&mut lines, line);
            if lines.len() >= MAX_SUMMARY_LINES {
                break;
            }
        }
    }

    if lines.is_empty() {
        lines.push("(no output)".into());
    }
    lines.truncate(MAX_SUMMARY_LINES);
    lines
}

pub async fn verify_project(
    workspace_root: &str,
    sandbox_enabled: bool,
) -> Result<VerifyProjectOutput, String> {
    verify_project_with_timeout(workspace_root, DEFAULT_VERIFY_TIMEOUT_MS, sandbox_enabled).await
}

pub async fn verify_project_with_timeout(
    workspace_root: &str,
    timeout_ms: u64,
    sandbox_enabled: bool,
) -> Result<VerifyProjectOutput, String> {
    let root = workspace_root_path(workspace_root)?;
    let (commands, skipped) = verification_plan(workspace_root)?;
    let mut results = Vec::new();

    for command_to_run in commands {
        if !executable_available(&command_to_run.executable) {
            results.push(VerificationResult {
                ecosystem: command_to_run.ecosystem,
                command: command_to_run.command,
                status: "SKIPPED_MISSING_TOOL".into(),
                success: false,
                exit_code: None,
                elapsed_ms: 0,
                summary: vec![format!(
                    "missing executable: {}",
                    command_to_run.executable
                )],
            });
            continue;
        }

        let result = if cfg!(windows) && command_to_run.ecosystem == "Node" {
            // Node package-manager launchers are commonly .cmd/.ps1 shims on
            // Windows. The command text is generated only from CatDesk's fixed
            // manager/script allowlist, so using the platform shell here does
            // not expose model-provided shell input.
            command::run_command(
                &command_to_run.command,
                &root,
                &root,
                sandbox_enabled,
                timeout_ms,
            )
            .await
        } else {
            command::run_program(
                &command_to_run.executable,
                &command_to_run.args,
                &root,
                &root,
                sandbox_enabled,
                timeout_ms,
            )
            .await
        };
        let status = if result.success {
            "PASSED"
        } else if result.timed_out {
            "TIMED_OUT"
        } else {
            "FAILED"
        };
        results.push(VerificationResult {
            ecosystem: command_to_run.ecosystem,
            command: command_to_run.command,
            status: status.into(),
            success: result.success,
            exit_code: result.exit_code,
            elapsed_ms: result.elapsed_ms,
            summary: summary_lines(&result),
        });
    }

    let status = if results.is_empty() {
        "NOT_CONFIGURED"
    } else if results
        .iter()
        .any(|result| matches!(result.status.as_str(), "FAILED" | "TIMED_OUT"))
    {
        "FAILED"
    } else if results
        .iter()
        .any(|result| result.status == "SKIPPED_MISSING_TOOL")
    {
        "PARTIAL"
    } else {
        "PASSED"
    };

    Ok(VerifyProjectOutput {
        success: status == "PASSED",
        status: status.into(),
        commands: results,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn test_workspace(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("catdesk-verification-{name}-{}", Uuid::new_v4()))
    }

    #[test]
    fn verification_plan_detects_rust_node_and_configured_python_commands() {
        let workspace = test_workspace("plan");
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .expect("write cargo");
        fs::write(workspace.join("pnpm-lock.yaml"), "").expect("write pnpm lock");
        fs::write(
            workspace.join("package.json"),
            r#"{"scripts":{"test":"echo test","build":"echo build"}}"#,
        )
        .expect("write package");
        fs::write(
            workspace.join("pyproject.toml"),
            "[project]\nname = \"demo\"\n[tool.pytest.ini_options]\n[tool.ruff]\n",
        )
        .expect("write pyproject");

        let (commands, skipped) =
            verification_plan(&workspace.to_string_lossy()).expect("build plan");
        let command_names = commands
            .iter()
            .map(|command| command.command.as_str())
            .collect::<Vec<_>>();
        assert!(command_names.contains(&"cargo fmt --check"));
        assert!(command_names.contains(&"cargo test"));
        assert!(command_names.contains(&"cargo build"));
        assert!(command_names.contains(&"pnpm run test"));
        assert!(command_names.contains(&"pnpm run build"));
        assert!(command_names.contains(&"pytest"));
        assert!(command_names.contains(&"ruff check ."));
        assert!(!command_names.contains(&"mypy ."));
        assert!(skipped.iter().any(|item| item.contains("lint")));
        assert!(skipped.iter().any(|item| item.contains("mypy")));

        let _ = fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn verify_project_reports_not_configured_for_empty_workspace() {
        let workspace = test_workspace("empty");
        fs::create_dir_all(&workspace).expect("create workspace");

        let output = verify_project(&workspace.to_string_lossy(), false)
            .await
            .expect("verify empty");

        assert_eq!(output.status, "NOT_CONFIGURED");
        assert!(!output.success);
        assert!(output.commands.is_empty());

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn summary_lines_captures_failure_near_end() {
        let mut stdout = String::new();
        for i in 0..100 {
            stdout.push_str(&format!("line {i}\n"));
        }
        stdout.push_str("test result: FAILED near end\n");
        let result = command::CommandResult {
            stdout,
            stderr: String::new(),
            success: false,
            exit_code: Some(1),
            elapsed_ms: 1,
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
        };

        let lines = summary_lines(&result);

        assert!(lines.iter().any(|line| line.contains("FAILED near end")));
    }
}
