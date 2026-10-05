use crate::command;
use crate::verification;
use serde::Serialize;
use std::collections::{BTreeSet, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const GIT_TIMEOUT_MS: u64 = 120_000;
const CONFIRMATION_TTL_SECS: u64 = 600;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitStatusSummary {
    pub branch: String,
    pub clean: bool,
    pub warn_on_main: bool,
    pub raw: String,
    pub summary: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffSummary {
    pub staged: Vec<String>,
    pub unstaged: Vec<String>,
    pub untracked: Vec<String>,
    pub deleted: Vec<String>,
    pub renamed: Vec<String>,
    pub ignored: Vec<String>,
    pub stat: String,
    pub summary: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitCommandOutput {
    pub success: bool,
    pub summary: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitCommitVerifiedOutput {
    pub success: bool,
    pub dry_run: bool,
    pub verification_status: String,
    pub verification_summary: String,
    pub staged_files: Vec<String>,
    pub confirmation_token: Option<String>,
    pub commit_preview: GitDiffSummary,
    pub commit: GitCommandOutput,
}

fn workspace_root_path(workspace_root: &str) -> Result<PathBuf, String> {
    Path::new(workspace_root)
        .canonicalize()
        .map(command::normalize_windows_verbatim_path)
        .map_err(|e| e.to_string())
}

async fn run_git(root: &Path, args: Vec<String>, sandbox_enabled: bool) -> command::CommandResult {
    command::run_program(
        "git",
        &args,
        root,
        root,
        sandbox_enabled,
        GIT_TIMEOUT_MS,
    )
    .await
}

fn command_output(result: command::CommandResult) -> GitCommandOutput {
    GitCommandOutput {
        success: result.success,
        summary: command::format_result(&result),
        stdout: result.stdout,
        stderr: result.stderr,
        exit_code: result.exit_code,
        timed_out: result.timed_out,
    }
}

async fn ensure_workspace_is_repo_root(
    root: &Path,
    sandbox_enabled: bool,
) -> Result<(), String> {
    let result = run_git(
        root,
        vec!["rev-parse".into(), "--show-toplevel".into()],
        sandbox_enabled,
    )
    .await;
    if !result.success {
        return Err(command::format_result(&result));
    }
    let top_level = Path::new(result.stdout.trim())
        .canonicalize()
        .map(command::normalize_windows_verbatim_path)
        .map_err(|e| format!("Failed to resolve git repository root: {e}"))?;
    if top_level != root {
        return Err(format!(
            "git_commit_verified requires the CatDesk workspace root to match the Git repository root.\nworkspace: {}\nrepository: {}",
            root.display(),
            top_level.display()
        ));
    }
    Ok(())
}

fn validate_branch_name(branch: &str) -> Result<(), String> {
    let branch = branch.trim();
    if branch.is_empty()
        || branch.starts_with('-')
        || branch.ends_with('/')
        || branch.contains("..")
        || branch.contains("@{")
    {
        return Err("Invalid branch name".into());
    }
    if !branch
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '-' | '_' | '.'))
    {
        return Err(
            "Branch names may contain only ASCII letters, numbers, '/', '-', '_', and '.'".into(),
        );
    }
    Ok(())
}

fn current_branch_from_status(raw: &str) -> String {
    raw.lines()
        .next()
        .and_then(|line| line.strip_prefix("## "))
        .map(|line| {
            let line = line.strip_prefix("No commits yet on ").unwrap_or(line);
            line.split("...")
                .next()
                .unwrap_or(line)
                .split_whitespace()
                .next()
                .unwrap_or(line)
                .to_string()
        })
        .filter(|branch| !branch.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn status_summary_text(branch: &str, clean: bool, warn_on_main: bool, raw: &str) -> String {
    let mut summary = format!(
        "branch: {branch}\nclean: {}\n",
        if clean { "yes" } else { "no" }
    );
    if warn_on_main {
        summary.push_str("warning: currently on main/master; create a feature branch before committing.\n");
    }
    if !raw.trim().is_empty() {
        summary.push('\n');
        summary.push_str(raw.trim());
    }
    summary
}

pub async fn status_summary(
    workspace_root: &str,
    sandbox_enabled: bool,
) -> Result<GitStatusSummary, String> {
    let root = workspace_root_path(workspace_root)?;
    let result = run_git(
        &root,
        vec!["status".into(), "--short".into(), "--branch".into()],
        sandbox_enabled,
    )
    .await;
    if !result.success {
        return Err(command::format_result(&result));
    }
    let raw = result.stdout;
    let branch = current_branch_from_status(&raw);
    let clean = raw.lines().count() <= 1;
    let warn_on_main = matches!(branch.as_str(), "main" | "master");
    let summary = status_summary_text(&branch, clean, warn_on_main, &raw);
    Ok(GitStatusSummary {
        branch,
        clean,
        warn_on_main,
        raw,
        summary,
    })
}

pub async fn create_feature_branch(
    workspace_root: &str,
    branch: &str,
    sandbox_enabled: bool,
) -> Result<GitCommandOutput, String> {
    validate_branch_name(branch)?;
    let root = workspace_root_path(workspace_root)?;
    let result = run_git(
        &root,
        vec!["switch".into(), "-c".into(), branch.trim().into()],
        sandbox_enabled,
    )
    .await;
    Ok(command_output(result))
}

fn parse_porcelain(
    raw: &str,
) -> (
    Vec<String>,
    Vec<String>,
    Vec<String>,
    Vec<String>,
    Vec<String>,
    Vec<String>,
) {
    let mut staged = BTreeSet::new();
    let mut unstaged = BTreeSet::new();
    let mut untracked = BTreeSet::new();
    let mut deleted = BTreeSet::new();
    let mut renamed = BTreeSet::new();
    let mut ignored = BTreeSet::new();

    for line in raw.lines().filter(|line| line.len() >= 3) {
        let status = &line[..2];
        let path = line[3..].to_string();
        if status == "??" {
            untracked.insert(path);
            continue;
        }
        if status == "!!" {
            ignored.insert(path);
            continue;
        }
        let mut chars = status.chars();
        let index = chars.next().unwrap_or(' ');
        let worktree = chars.next().unwrap_or(' ');
        if index != ' ' {
            staged.insert(path.clone());
        }
        if worktree != ' ' {
            unstaged.insert(path.clone());
        }
        if index == 'D' || worktree == 'D' {
            deleted.insert(path.clone());
        }
        if index == 'R' || worktree == 'R' || path.contains(" -> ") {
            renamed.insert(path);
        }
    }

    (
        staged.into_iter().collect(),
        unstaged.into_iter().collect(),
        untracked.into_iter().collect(),
        deleted.into_iter().collect(),
        renamed.into_iter().collect(),
        ignored.into_iter().collect(),
    )
}

fn diff_summary_text(output: &GitDiffSummary) -> String {
    let mut summary = String::new();
    for (label, files) in [
        ("staged", &output.staged),
        ("unstaged", &output.unstaged),
        ("untracked", &output.untracked),
        ("deleted", &output.deleted),
        ("renamed", &output.renamed),
        ("ignored", &output.ignored),
    ] {
        summary.push_str(label);
        summary.push_str(":\n");
        if files.is_empty() {
            summary.push_str("- none\n");
        } else {
            for file in files {
                summary.push_str("- ");
                summary.push_str(file);
                summary.push('\n');
            }
        }
    }
    if !output.stat.trim().is_empty() {
        summary.push_str("\nstat:\n");
        summary.push_str(output.stat.trim());
        summary.push('\n');
    }
    summary
}

pub async fn diff_summary(
    workspace_root: &str,
    include_ignored: bool,
    sandbox_enabled: bool,
) -> Result<GitDiffSummary, String> {
    let root = workspace_root_path(workspace_root)?;
    let mut status_args = vec!["status".to_string(), "--porcelain".to_string()];
    if include_ignored {
        status_args.push("--ignored".into());
    }
    let status = run_git(&root, status_args, sandbox_enabled).await;
    if !status.success {
        return Err(command::format_result(&status));
    }

    let stat = run_git(
        &root,
        vec!["diff".into(), "--stat".into(), "HEAD".into()],
        sandbox_enabled,
    )
    .await;
    let stat_text = if stat.success {
        stat.stdout
    } else {
        let fallback = run_git(
            &root,
            vec!["diff".into(), "--stat".into()],
            sandbox_enabled,
        )
        .await;
        if fallback.success {
            fallback.stdout
        } else {
            return Err(command::format_result(&stat));
        }
    };

    let (staged, unstaged, untracked, deleted, renamed, ignored) = parse_porcelain(&status.stdout);
    let mut output = GitDiffSummary {
        staged,
        unstaged,
        untracked,
        deleted,
        renamed,
        ignored,
        stat: stat_text,
        summary: String::new(),
    };
    output.summary = diff_summary_text(&output);
    Ok(output)
}

fn looks_like_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\'))
        || path.starts_with("\\\\")
}

fn validate_stage_path(path: &str) -> Result<(), String> {
    let trimmed = path.trim();
    if trimmed.is_empty()
        || trimmed.chars().any(char::is_control)
        || trimmed == "."
        || trimmed == "./"
        || trimmed == ".\\"
        || trimmed.ends_with('/')
        || trimmed.ends_with('\\')
        || trimmed.starts_with('-')
        || Path::new(trimmed).is_absolute()
        || looks_like_windows_absolute(trimmed)
    {
        return Err(format!("Invalid staged file path: {path}"));
    }

    let normalized = trimmed.replace('\\', "/");
    if normalized.split('/').any(|part| part == "..") {
        return Err(format!("Parent traversal is not allowed in staged file path: {path}"));
    }
    let first = normalized
        .split('/')
        .find(|part| !part.is_empty() && *part != ".")
        .unwrap_or_default();
    if first.eq_ignore_ascii_case(".git") || first.eq_ignore_ascii_case(".catdesk") {
        return Err(format!("Protected path cannot be staged: {path}"));
    }
    Ok(())
}

fn validate_stage_files(root: &Path, files: &[String]) -> Result<(), String> {
    for file in files {
        validate_stage_path(file)?;
        let candidate = command::resolve_workspace_path(&root.to_string_lossy(), Some(file))?;
        if candidate.is_dir() {
            return Err(format!(
                "Invalid staged file path: {file}. git_commit_verified requires explicit files, not directories."
            ));
        }
    }
    Ok(())
}

fn literal_pathspec(path: &str) -> String {
    format!(":(literal){}", path.trim())
}

fn sorted_unique(files: Vec<String>) -> Vec<String> {
    files
        .into_iter()
        .map(|file| {
            file.trim()
                .replace('\\', "/")
                .split('/')
                .filter(|part| !part.is_empty() && *part != ".")
                .collect::<Vec<_>>()
                .join("/")
        })
        .filter(|file| !file.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

async fn staged_files(root: &Path, sandbox_enabled: bool) -> Result<Vec<String>, String> {
    let result = run_git(
        root,
        vec![
            "diff".into(),
            "--cached".into(),
            "--name-only".into(),
            "-z".into(),
        ],
        sandbox_enabled,
    )
    .await;
    if !result.success {
        return Err(command::format_result(&result));
    }
    if result.stdout_truncated {
        return Err(
            "Refusing verified commit because the staged file list exceeded CatDesk's capture limit"
                .into(),
        );
    }
    Ok(sorted_unique(
        result
            .stdout
            .split('\0')
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
    ))
}

fn unexpected_staged_files(staged: &[String], approved: &[String]) -> Vec<String> {
    let approved = approved.iter().cloned().collect::<BTreeSet<_>>();
    staged
        .iter()
        .filter(|file| !approved.contains(*file))
        .cloned()
        .collect()
}

async fn commit_preview_state(root: &Path, sandbox_enabled: bool) -> Result<String, String> {
    let head = run_git(
        root,
        vec!["rev-parse".into(), "--verify".into(), "HEAD".into()],
        sandbox_enabled,
    )
    .await;
    let head = if head.success {
        head.stdout.trim().to_string()
    } else {
        "<unborn>".to_string()
    };

    let status = run_git(
        root,
        vec!["status".into(), "--porcelain=v1".into(), "-z".into()],
        sandbox_enabled,
    )
    .await;
    if !status.success {
        return Err(command::format_result(&status));
    }
    if status.stdout_truncated {
        return Err(
            "Refusing verified commit because git status exceeded CatDesk's capture limit".into(),
        );
    }
    let staged = run_git(
        root,
        vec![
            "diff".into(),
            "--cached".into(),
            "--raw".into(),
            "--abbrev=40".into(),
            "-z".into(),
        ],
        sandbox_enabled,
    )
    .await;
    if !staged.success {
        return Err(command::format_result(&staged));
    }
    if staged.stdout_truncated {
        return Err(
            "Refusing verified commit because staged raw state exceeded CatDesk's capture limit"
                .into(),
        );
    }
    Ok(format!(
        "workspace:\n{}\n\nhead:\n{}\n\nstatus:\n{}\n\nstaged raw:\n{}",
        root.display(),
        head,
        status.stdout,
        staged.stdout
    ))
}

fn commit_confirmation_fingerprint(
    branch: &str,
    message: &str,
    files: &[String],
    verification_status: &str,
    preview_state: &str,
    add_catdesk_co_author: bool,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    branch.hash(&mut hasher);
    message.trim().hash(&mut hasher);
    files.hash(&mut hasher);
    verification_status.hash(&mut hasher);
    preview_state.hash(&mut hasher);
    add_catdesk_co_author.hash(&mut hasher);
    hasher.finish()
}

fn commit_confirmation_token(
    branch: &str,
    message: &str,
    files: &[String],
    verification_status: &str,
    preview_state: &str,
    add_catdesk_co_author: bool,
) -> Result<String, String> {
    let issued_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let fingerprint = commit_confirmation_fingerprint(
        branch,
        message,
        files,
        verification_status,
        preview_state,
        add_catdesk_co_author,
    );
    Ok(format!("commit:{issued_at}:{fingerprint:016x}"))
}

fn validate_commit_confirmation_token(
    token: Option<&str>,
    branch: &str,
    message: &str,
    files: &[String],
    verification_status: &str,
    preview_state: &str,
    add_catdesk_co_author: bool,
) -> Result<(), String> {
    let Some(token) = token.filter(|value| !value.trim().is_empty()) else {
        return Err(
            "Run git_commit_verified with dry_run=true first and pass the returned commit_confirmation_token."
                .into(),
        );
    };
    let mut parts = token.split(':');
    if parts.next() != Some("commit") {
        return Err("Invalid commit confirmation token.".into());
    }
    let issued_at = parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| "Invalid commit confirmation token.".to_string())?;
    let fingerprint = parts
        .next()
        .and_then(|value| u64::from_str_radix(value, 16).ok())
        .ok_or_else(|| "Invalid commit confirmation token.".to_string())?;
    if parts.next().is_some() {
        return Err("Invalid commit confirmation token.".into());
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    if now.saturating_sub(issued_at) > CONFIRMATION_TTL_SECS {
        return Err("Commit confirmation token expired; run dry_run=true again.".into());
    }
    let expected = commit_confirmation_fingerprint(
        branch,
        message,
        files,
        verification_status,
        preview_state,
        add_catdesk_co_author,
    );
    if fingerprint != expected {
        return Err("Commit confirmation token does not match the current staged preview.".into());
    }
    Ok(())
}

fn blocked_commit_output(summary: impl Into<String>) -> GitCommandOutput {
    GitCommandOutput {
        success: false,
        summary: summary.into(),
        stdout: String::new(),
        stderr: String::new(),
        exit_code: None,
        timed_out: false,
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn commit_verified_changes(
    workspace_root: &str,
    message: &str,
    files: Vec<String>,
    allow_failed_verification: bool,
    allow_partial_verification: bool,
    allow_main: bool,
    dry_run: bool,
    provided_confirmation_token: Option<&str>,
    sandbox_enabled: bool,
    add_catdesk_co_author: bool,
) -> Result<GitCommitVerifiedOutput, String> {
    if message.trim().is_empty() {
        return Err("Commit message must not be empty".into());
    }
    if command::contains_catdesk_co_author_marker(message) {
        return Err(
            "Do not include a CatDesk Co-Authored-By trailer in the commit message; CatDesk manages that setting automatically."
                .into(),
        );
    }
    if files.is_empty() {
        return Err("git_commit_verified requires an explicit non-empty files list".into());
    }

    let root = workspace_root_path(workspace_root)?;
    ensure_workspace_is_repo_root(&root, sandbox_enabled).await?;
    for file in &files {
        validate_stage_path(file)?;
    }
    let files = sorted_unique(files);
    validate_stage_files(&root, &files)?;
    let status = status_summary(workspace_root, sandbox_enabled).await?;
    if matches!(status.branch.as_str(), "main" | "master") && !allow_main {
        return Err(
            "Refusing to commit on main/master. Use git_create_feature_branch first, or pass allow_main=true for an explicit override."
                .into(),
        );
    }

    let verification = verification::verify_project(workspace_root, sandbox_enabled).await?;
    let verification_summary = verification.render_text();
    let verification_allowed = match verification.status.as_str() {
        "PASSED" => true,
        "PARTIAL" => allow_partial_verification || allow_failed_verification,
        "FAILED" | "NOT_CONFIGURED" => allow_failed_verification,
        _ => false,
    };
    if !verification_allowed {
        return Ok(GitCommitVerifiedOutput {
            success: false,
            dry_run,
            verification_status: verification.status,
            verification_summary,
            staged_files: files,
            confirmation_token: None,
            commit_preview: diff_summary(workspace_root, false, sandbox_enabled).await?,
            commit: blocked_commit_output("verification did not pass; commit was not created"),
        });
    }

    let already_staged = staged_files(&root, sandbox_enabled).await?;
    let unexpected = unexpected_staged_files(&already_staged, &files);
    if !unexpected.is_empty() {
        return Err(format!(
            "Refusing verified commit because unrelated files are already staged: {}",
            unexpected.join(", ")
        ));
    }

    if dry_run {
        let mut add_args = vec!["add".to_string(), "--".to_string()];
        add_args.extend(files.iter().map(|file| literal_pathspec(file)));
        let add = run_git(&root, add_args, sandbox_enabled).await;
        if !add.success {
            return Err(command::format_result(&add));
        }

        let staged_after_add = staged_files(&root, sandbox_enabled).await?;
        if staged_after_add != files {
            return Err(format!(
                "Refusing verified commit because staged files do not exactly match requested files.\nrequested: {}\nstaged: {}",
                files.join(", "),
                staged_after_add.join(", ")
            ));
        }

        let preview = diff_summary(workspace_root, false, sandbox_enabled).await?;
        let preview_state = commit_preview_state(&root, sandbox_enabled).await?;
        let token = commit_confirmation_token(
            &status.branch,
            message,
            &files,
            &verification.status,
            &preview_state,
            add_catdesk_co_author,
        )?;
        return Ok(GitCommitVerifiedOutput {
            success: true,
            dry_run: true,
            verification_status: verification.status,
            verification_summary,
            staged_files: files,
            confirmation_token: Some(token),
            commit_preview: preview,
            commit: GitCommandOutput {
                success: true,
                summary: "dry run: verification accepted and requested files are staged; no commit was created"
                    .into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
                timed_out: false,
            },
        });
    }

    if already_staged != files {
        return Err(format!(
            "Refusing verified commit because the staged files no longer match the dry-run preview.\nrequested: {}\nstaged: {}\nRun dry_run=true again.",
            files.join(", "),
            already_staged.join(", ")
        ));
    }

    let preview = diff_summary(workspace_root, false, sandbox_enabled).await?;
    let preview_state = commit_preview_state(&root, sandbox_enabled).await?;
    validate_commit_confirmation_token(
        provided_confirmation_token,
        &status.branch,
        message,
        &files,
        &verification.status,
        &preview_state,
        add_catdesk_co_author,
    )?;

    let mut commit_args = vec!["commit".to_string()];
    if add_catdesk_co_author {
        commit_args.push("--trailer".into());
        commit_args.push(command::CATDESK_CO_AUTHOR_TRAILER.into());
    }
    commit_args.push("-m".into());
    commit_args.push(message.trim().into());
    let commit = run_git(&root, commit_args, sandbox_enabled).await;

    Ok(GitCommitVerifiedOutput {
        success: commit.success,
        dry_run: false,
        verification_status: verification.status,
        verification_summary,
        staged_files: files,
        confirmation_token: None,
        commit_preview: preview,
        commit: command_output(commit),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_and_warns_on_main() {
        let raw = "## main...origin/main\n M src/main.rs\n";
        let branch = current_branch_from_status(raw);
        assert_eq!(branch, "main");
        assert_eq!(
            current_branch_from_status("## No commits yet on main\n"),
            "main"
        );
        let summary = status_summary_text(&branch, false, true, raw);
        assert!(summary.contains("warning: currently on main/master"));
    }

    #[test]
    fn validates_feature_branch_names() {
        assert!(validate_branch_name("feature/git-workflow").is_ok());
        assert!(validate_branch_name("-bad").is_err());
        assert!(validate_branch_name("bad name").is_err());
        assert!(validate_branch_name("bad..name").is_err());
        assert!(validate_branch_name("bad@{name").is_err());
    }

    #[test]
    fn parses_porcelain_sections() {
        let raw =
            "M  staged.rs\n M unstaged.rs\n?? new file.rs\nD  deleted.rs\nR  old.rs -> new.rs\n";
        let (staged, unstaged, untracked, deleted, renamed, ignored) = parse_porcelain(raw);

        assert!(staged.contains(&"staged.rs".to_string()));
        assert!(unstaged.contains(&"unstaged.rs".to_string()));
        assert!(untracked.contains(&"new file.rs".to_string()));
        assert!(deleted.contains(&"deleted.rs".to_string()));
        assert!(renamed.contains(&"old.rs -> new.rs".to_string()));
        assert!(ignored.is_empty());
    }

    #[test]
    fn validates_explicit_stage_paths() {
        assert!(validate_stage_path("src/file with spaces.rs").is_ok());
        assert!(validate_stage_path("src/file[1].rs").is_ok());
        assert!(validate_stage_path(".").is_err());
        assert!(validate_stage_path("./").is_err());
        assert!(validate_stage_path("src/").is_err());
        assert!(validate_stage_path("-bad").is_err());
        assert!(validate_stage_path("C:/secret.txt").is_err());
        assert!(validate_stage_path("src/../secret.txt").is_err());
        assert!(validate_stage_path("src/bad\nname.rs").is_err());
        assert!(validate_stage_path(".git/config").is_err());
        assert!(validate_stage_path("./.git/config").is_err());
        assert!(validate_stage_path(".catdesk/session.md").is_err());
        assert!(validate_stage_path("./.catdesk/session.md").is_err());
    }

    #[test]
    fn normalizes_explicit_stage_paths_before_exact_staged_comparison() {
        assert_eq!(
            sorted_unique(vec![
                "./src//main.rs".into(),
                "src\\main.rs".into(),
                "./src/lib.rs".into(),
            ]),
            vec!["src/lib.rs".to_string(), "src/main.rs".to_string()]
        );
    }

    #[test]
    fn commit_confirmation_token_tracks_preview_state_and_attribution() {
        let files = vec!["src/main.rs".to_string()];
        let preview = "status:\nM  src/main.rs\n\nstaged raw:\n:blob";
        let token = commit_confirmation_token(
            "feature/demo",
            "Update demo",
            &files,
            "PASSED",
            preview,
            true,
        )
        .expect("token");
        assert!(
            validate_commit_confirmation_token(
                Some(&token),
                "feature/demo",
                "Update demo",
                &files,
                "PASSED",
                preview,
                true,
            )
            .is_ok()
        );
        assert!(
            validate_commit_confirmation_token(
                Some(&token),
                "feature/demo",
                "Different message",
                &files,
                "PASSED",
                preview,
                true,
            )
            .is_err()
        );
        assert!(
            validate_commit_confirmation_token(
                Some(&token),
                "feature/demo",
                "Update demo",
                &files,
                "PASSED",
                preview,
                false,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn commit_call_does_not_restage_changes_after_dry_run_preview() {
        use std::process::Command;
        use uuid::Uuid;

        if Command::new("git").arg("--version").output().is_err() {
            return;
        }

        let root = std::env::temp_dir().join(format!(
            "catdesk-git-workflow-no-restage-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("create git workspace");
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .expect("run git")
        };
        assert!(git(&["init"]).status.success());
        assert!(git(&["config", "user.email", "catdesk@example.invalid"]).status.success());
        assert!(git(&["config", "user.name", "CatDesk Test"]).status.success());

        std::fs::write(root.join("notes.txt"), "preview\n").expect("write preview file");
        let root_text = root.to_string_lossy().into_owned();
        let preview = commit_verified_changes(
            &root_text,
            "test preview",
            vec!["notes.txt".into()],
            true,
            false,
            true,
            true,
            None,
            false,
            false,
        )
        .await
        .expect("dry-run commit preview");
        let token = preview
            .confirmation_token
            .expect("dry-run confirmation token");

        let staged_before = git(&["show", ":notes.txt"]);
        assert!(staged_before.status.success());
        assert_eq!(String::from_utf8_lossy(&staged_before.stdout), "preview\n");

        std::fs::write(root.join("notes.txt"), "changed after preview\n")
            .expect("change working tree after preview");
        let blocked = commit_verified_changes(
            &root_text,
            "test preview",
            vec!["notes.txt".into()],
            true,
            false,
            true,
            false,
            Some(&token),
            false,
            false,
        )
        .await
        .expect_err("changed working tree must invalidate confirmation");

        assert!(blocked.contains("confirmation token"));
        let staged_after = git(&["show", ":notes.txt"]);
        assert!(staged_after.status.success());
        assert_eq!(String::from_utf8_lossy(&staged_after.stdout), "preview\n");
        assert!(
            !git(&["rev-parse", "--verify", "HEAD"]).status.success(),
            "blocked confirmation must not create a commit"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
