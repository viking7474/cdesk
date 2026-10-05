use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::state::{CloudflareMode, SharedState};

/// Extract an `https://...` URL from a cloudflared log line, if it matches `needle`.
fn extract_url(line: &str, needle: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '"' || c == '|')
        .unwrap_or(rest.len());
    let url = rest[..end].trim_end_matches(['.', ',', ')']).to_string();
    if url.contains(needle) { Some(url) } else { None }
}

/// Start a Cloudflare tunnel for the local MCP server.
///
/// Spawns the external `cloudflared` binary so it can run in parallel with the
/// ngrok tunnel. Supports both quick tunnels (a random `trycloudflare.com` URL)
/// and named tunnels bound to the user's own domain (via a tunnel token or a
/// hostname + local URL).
pub async fn start(state: SharedState) -> Result<(), String> {
    let (running, port, mode, domain, token) = {
        let app = state.lock().await;
        (
            app.cloudflare_running,
            app.port,
            app.cloudflare_mode,
            app.cloudflare_domain.clone(),
            app.cloudflare_tunnel_token.clone(),
        )
    };

    if running {
        return Ok(());
    }

    let forwards_to = format!("http://127.0.0.1:{port}");
    let mut cmd = Command::new("cloudflared");
    cmd.arg("tunnel").arg("--no-autoupdate");

    let expected_url: Option<String> = match mode {
        CloudflareMode::Named => {
            let token = token.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
            let domain = domain
                .map(|d| {
                    d.trim()
                        .trim_start_matches("https://")
                        .trim_start_matches("http://")
                        .trim_end_matches('/')
                        .to_string()
                })
                .filter(|d| !d.is_empty());
            if let Some(token) = token {
                cmd.arg("run").arg("--token").arg(&token);
                domain.map(|d| "https://".to_string() + &d)
            } else if let Some(domain) = domain {
                cmd.arg("--hostname")
                    .arg(&domain)
                    .arg("--url")
                    .arg(&forwards_to);
                Some("https://".to_string() + &domain)
            } else {
                return Err(
                    "Cloudflare named tunnel requires a domain or a tunnel token".to_string(),
                );
            }
        }
        CloudflareMode::Quick => {
            cmd.arg("--url").arg(&forwards_to);
            None
        }
    };

    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| {
        format!("failed to launch cloudflared ({e}). Install it and ensure it is on PATH.")
    })?;

    let stderr = child.stderr.take();
    let stdout = child.stdout.take();

    {
        let mut app = state.lock().await;
        app.cloudflare_child = Some(child);
        app.cloudflare_running = true;
        app.cloudflare_url = expected_url.clone();
        app.log("INFO", "Cloudflare tunnel starting".to_string());
    }

    // Drain stdout so a full pipe never blocks cloudflared.
    if let Some(stdout) = stdout {
        let state_out = state.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(url) = extract_url(&line, "trycloudflare.com") {
                    let mut app = state_out.lock().await;
                    app.cloudflare_url = Some(url.clone());
                    app.log("INFO", format!("Cloudflare tunnel ready: {url}"));
                }
            }
        });
    }

    // Watch stderr for the public URL and for process exit.
    let watcher_state = state.clone();
    let task = tokio::spawn(async move {
        if let Some(stderr) = stderr {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(url) = extract_url(&line, "trycloudflare.com") {
                    let mut app = watcher_state.lock().await;
                    app.cloudflare_url = Some(url.clone());
                    app.log("INFO", format!("Cloudflare tunnel ready: {url}"));
                }
            }
        }
        // stderr closed => the cloudflared process has exited.
        let mut app = watcher_state.lock().await;
        app.cloudflare_running = false;
        app.log("WARN", "Cloudflare tunnel stopped".to_string());
    });

    {
        let mut app = state.lock().await;
        app.cloudflare_task = Some(task);
    }

    Ok(())
}
