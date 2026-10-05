use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::state::{AppState, CloudflareMode, ServerUiEvent, SharedState};

/// Static control UI served under the current secret MCP prefix.
pub const CONTROL_UI_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>CatDesk Control</title>
<style>
 body { font-family: system-ui, sans-serif; margin: 2rem; background:#0f1115; color:#e6e6e6; }
 h1 { font-size:1.4rem; }
 table { border-collapse: collapse; width:100%; margin-top:1rem; }
 th,td { border:1px solid #333; padding:.4rem .6rem; text-align:left; font-size:.9rem; }
 form { margin-top:1rem; padding:1rem; border:1px solid #333; border-radius:8px; max-width:560px; }
 label { display:block; margin:.4rem 0 .1rem; font-size:.85rem; }
 input,select { width:100%; padding:.35rem; background:#1a1d23; color:#e6e6e6; border:1px solid #333; border-radius:4px; }
 .row { display:flex; gap:1rem; }
 .row > div { flex:1; }
 button { margin-top:.8rem; padding:.5rem 1rem; background:#3b82f6; color:#fff; border:0; border-radius:6px; cursor:pointer; }
 button.danger { background:#ef4444; margin:0; }
 .chk { display:flex; align-items:center; gap:.4rem; margin-top:.6rem; }
 .chk input { width:auto; }
 a { color:#60a5fa; }
</style>
</head>
<body>
<h1>CatDesk - Multi-instance control</h1>
<p>Run additional MCP servers, each on its own port and workspace, with optional ngrok and Cloudflare tunnels in parallel.</p>
<div id="list">Loading...</div>
<form id="add">
 <label>Instance name</label>
 <input name="name" type="text" placeholder="my-workspace" />
 <div class="row">
  <div><label>Port</label><input name="port" type="number" min="1" max="65535" placeholder="3201" required /></div>
  <div><label>Workspace path</label><input name="workspace" type="text" placeholder="/path/to/workspace" required /></div>
 </div>
 <div class="chk"><input type="checkbox" name="ngrok" id="ngrok" /><label for="ngrok" style="margin:0">Enable ngrok</label></div>
 <div class="chk"><input type="checkbox" name="cloudflare" id="cloudflare" /><label for="cloudflare" style="margin:0">Enable Cloudflare tunnel</label></div>
 <label>Cloudflare mode</label>
 <select name="cloudflareMode"><option value="quick">Quick (trycloudflare.com)</option><option value="named">Named (your domain)</option></select>
 <label>Cloudflare domain (named)</label>
 <input name="cloudflareDomain" type="text" placeholder="mcp.example.com" />
 <label>Cloudflare tunnel token (named)</label>
 <input name="cloudflareToken" type="text" placeholder="eyJ..." />
 <button type="submit">Start instance</button>
</form>
<script>
const controlBase=window.location.pathname.replace(/\/$/,'');
async function refresh(){
 try {
  const r = await fetch(controlBase+'/api/instances');
  const d = await r.json();
  const items = (d.instances)||[];
  if(!items.length){ document.getElementById('list').innerHTML='<p>No extra instances running.</p>'; return; }
  let h='<table><tr><th>ID</th><th>Name</th><th>Port</th><th>Workspace</th><th>MCP path</th><th>ngrok</th><th>Cloudflare</th><th></th></tr>';
  for(const i of items){
   const ng = i.ngrok_url ? ('<a href="'+i.ngrok_url+'">'+i.ngrok_url+'</a>') : (i.ngrok_running?'starting...':'off');
   const cf = i.cloudflare_url ? ('<a href="'+i.cloudflare_url+'">'+i.cloudflare_url+'</a>') : (i.cloudflare_running?'starting...':'off');
   h+='<tr><td>'+i.id+'</td><td>'+(i.name||'')+'</td><td>'+i.port+'</td><td>'+i.workspace_root+'</td><td>'+i.mcp_path+'</td><td>'+ng+'</td><td>'+cf+'</td><td><button class="danger" onclick="removeInstance('+i.id+')">Remove</button></td></tr>';
  }
  h+='</table>';
  document.getElementById('list').innerHTML=h;
 } catch(e){ document.getElementById('list').innerHTML='<p>Failed to load instances.</p>'; }
}
async function removeInstance(id){
 await fetch(controlBase+'/api/instances/'+id,{method:'DELETE'});
 refresh();
}
document.getElementById('add').addEventListener('submit',async(e)=>{
 e.preventDefault();
 const f=new FormData(e.target);
 const body=new URLSearchParams();
 for(const [k,v] of f.entries()) body.append(k,v);
 body.set('ngrok', f.get('ngrok')?'on':'off');
 body.set('cloudflare', f.get('cloudflare')?'on':'off');
 const r=await fetch(controlBase+'/api/instances',{method:'POST',headers:{'content-type':'application/x-www-form-urlencoded'},body});
 const d=await r.json();
 if(!d.ok){ alert('Error: '+(d.error||'unknown')); }
 else if(d.instance && (d.instance.ngrok_error || d.instance.cloudflare_error)){
  let m='Instance started, but a tunnel failed:';
  if(d.instance.ngrok_error) m+='\n- ngrok: '+d.instance.ngrok_error;
  if(d.instance.cloudflare_error) m+='\n- Cloudflare: '+d.instance.cloudflare_error;
  alert(m);
 }
 e.target.reset();
 refresh();
});
refresh();
setInterval(refresh,4000);
</script>
</body>
</html>
"##;

/// Parameters for launching a new worker instance.
#[derive(Clone)]
pub struct InstanceSpec {
    pub name: String,
    pub port: u16,
    pub workspace_root: String,
    pub enable_ngrok: bool,
    pub enable_cloudflare: bool,
    pub cloudflare_mode: String,
    pub cloudflare_domain: Option<String>,
    pub cloudflare_token: Option<String>,
}

/// Serializable snapshot of a worker instance for the control API.
#[derive(Clone, Serialize)]
pub struct InstanceInfo {
    pub id: u64,
    pub name: String,
    pub port: u16,
    pub workspace_root: String,
    pub mcp_path: String,
    pub server_running: bool,
    pub ngrok_running: bool,
    pub ngrok_url: Option<String>,
    pub cloudflare_running: bool,
    pub cloudflare_url: Option<String>,
    pub ngrok_error: Option<String>,
    pub cloudflare_error: Option<String>,
}

struct WorkerInstance {
    id: u64,
    spec: InstanceSpec,
    port: u16,
    workspace_root: String,
    state: SharedState,
    server_handle: JoinHandle<()>,
    ui_task: JoinHandle<()>,
}

/// On-disk registry so instances can be restored on the next launch.
#[derive(Clone, Default, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    instances: Vec<PersistedInstance>,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedInstance {
    id: u64,
    #[serde(default)]
    name: String,
    port: u16,
    workspace_root: String,
    #[serde(default)]
    enable_ngrok: bool,
    #[serde(default)]
    enable_cloudflare: bool,
    #[serde(default = "default_cloudflare_mode")]
    cloudflare_mode: String,
    #[serde(default)]
    cloudflare_domain: Option<String>,
    #[serde(default)]
    cloudflare_token: Option<String>,
}

fn default_cloudflare_mode() -> String {
    "quick".to_string()
}

/// Registry that owns every additional (non-primary) MCP server instance.
pub struct InstanceManager {
    next_id: AtomicU64,
    instances: Mutex<HashMap<u64, WorkerInstance>>,
}

impl Default for InstanceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl InstanceManager {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            instances: Mutex::new(HashMap::new()),
        }
    }

    /// Snapshot every running instance for display.
    pub async fn list(&self) -> Vec<InstanceInfo> {
        let map = self.instances.lock().await;
        let mut out = Vec::with_capacity(map.len());
        for inst in map.values() {
            let app = inst.state.lock().await;
            out.push(InstanceInfo {
                id: inst.id,
                name: inst.spec.name.clone(),
                port: inst.port,
                workspace_root: inst.workspace_root.clone(),
                mcp_path: app.mcp_path(),
                server_running: app.server_running,
                ngrok_running: app.ngrok_running,
                ngrok_url: app.ngrok_url.clone(),
                cloudflare_running: app.cloudflare_running,
                cloudflare_url: app.cloudflare_url.clone(),
                ngrok_error: None,
                cloudflare_error: None,
            });
        }
        out.sort_by_key(|i| i.id);
        out
    }

    /// Create, bind, and start a new worker instance.
    pub async fn add(&self, spec: InstanceSpec) -> Result<InstanceInfo, String> {
        self.add_inner(spec, None).await
    }

    /// Restore previously-saved instances from the on-disk registry.
    pub async fn restore_saved(&self) {
        let file = match Self::load_registry() {
            Ok(file) => file,
            Err(e) => {
                eprintln!("catdesk: failed to load instance registry: {e}");
                return;
            }
        };
        let mut max_id = 0u64;
        for p in file.instances {
            max_id = max_id.max(p.id);
            let spec = InstanceSpec {
                name: p.name,
                port: p.port,
                workspace_root: p.workspace_root,
                enable_ngrok: p.enable_ngrok,
                enable_cloudflare: p.enable_cloudflare,
                cloudflare_mode: p.cloudflare_mode,
                cloudflare_domain: p.cloudflare_domain,
                cloudflare_token: p.cloudflare_token,
            };
            if let Err(e) = self.add_inner(spec, Some(p.id)).await {
                eprintln!("catdesk: failed to restore instance {}: {e}", p.id);
            }
        }
        if max_id >= self.next_id.load(Ordering::SeqCst) {
            self.next_id.store(max_id + 1, Ordering::SeqCst);
        }
    }

    async fn add_inner(
        &self,
        mut spec: InstanceSpec,
        restore_id: Option<u64>,
    ) -> Result<InstanceInfo, String> {
        let id = match restore_id {
            Some(id) => id,
            None => self.next_id.fetch_add(1, Ordering::SeqCst),
        };
        if spec.name.trim().is_empty() {
            spec.name = format!("instance-{id}");
        }
        let config_path = instance_config_path(id)?;
        let mut app_state =
            AppState::new_with_config_path(spec.port, spec.workspace_root.clone(), config_path)
                .map_err(|e| format!("failed to initialize instance state: {e}"))?;
        app_state.cloudflare_mode = match spec.cloudflare_mode.as_str() {
            "named" => CloudflareMode::Named,
            _ => CloudflareMode::Quick,
        };
        app_state.cloudflare_domain = spec
            .cloudflare_domain
            .clone()
            .filter(|s| !s.trim().is_empty());
        app_state.cloudflare_tunnel_token =
            spec.cloudflare_token.clone().filter(|s| !s.trim().is_empty());
        app_state.cloudflare_enabled = spec.enable_cloudflare;
        app_state
            .persist_state()
            .map_err(|e| format!("failed to persist instance config: {e}"))?;

        let state: SharedState = Arc::new(Mutex::new(app_state));

        let (ui_tx, mut ui_rx) =
            tokio::sync::mpsc::unbounded_channel::<ServerUiEvent>();
        let ui_state = state.clone();
        let ui_task = tokio::spawn(async move {
            while let Some(event) = ui_rx.recv().await {
                let mut app = ui_state.lock().await;
                app.apply_server_ui_event(event);
            }
        });

        let (mcp_path, command_jobs) = {
            let app = state.lock().await;
            (app.mcp_path(), app.command_jobs.clone())
        };

        let router = crate::server::router(
            state.clone(),
            None,
            command_jobs,
            mcp_path.clone(),
            ui_tx,
        );
        let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", spec.port))
            .await
            .map_err(|e| format!("failed to bind port {}: {e}", spec.port))?;
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        {
            let mut app = state.lock().await;
            app.server_running = true;
            app.log("INFO", format!("Instance {id} started on port {}", spec.port));
        }

        let mut ngrok_error: Option<String> = None;
        if spec.enable_ngrok {
            if let Err(e) = crate::ngrok::start(state.clone()).await {
                let mut app = state.lock().await;
                app.log("ERROR", format!("instance {id} ngrok: {e}"));
                ngrok_error = Some(e);
            }
        }
        let mut cloudflare_error: Option<String> = None;
        if spec.enable_cloudflare {
            if let Err(e) = crate::cloudflare::start(state.clone()).await {
                let mut app = state.lock().await;
                app.log("ERROR", format!("instance {id} cloudflare: {e}"));
                cloudflare_error = Some(e);
            }
        }

        // For named tunnels, present the MCP link as https://<domain>/<instance-name>.
        if spec.enable_cloudflare && cloudflare_error.is_none() && spec.cloudflare_mode == "named" {
            let mut app = state.lock().await;
            if let Some(domain) = app.cloudflare_domain.clone() {
                let domain = domain
                    .trim()
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .trim_end_matches('/')
                    .to_string();
                if !domain.is_empty() {
                    let slug = slugify(&spec.name);
                    let base = String::from("https://") + &domain;
                    app.cloudflare_url = Some(if slug.is_empty() {
                        base
                    } else {
                        base.clone() + "/" + &slug
                    });
                }
            }
        }

        let info = {
            let app = state.lock().await;
            InstanceInfo {
                id,
                name: spec.name.clone(),
                port: spec.port,
                workspace_root: spec.workspace_root.clone(),
                mcp_path,
                server_running: app.server_running,
                ngrok_running: app.ngrok_running,
                ngrok_url: app.ngrok_url.clone(),
                cloudflare_running: app.cloudflare_running,
                cloudflare_url: app.cloudflare_url.clone(),
                ngrok_error,
                cloudflare_error,
            }
        };

        let worker = WorkerInstance {
            id,
            spec: spec.clone(),
            port: spec.port,
            workspace_root: spec.workspace_root.clone(),
            state,
            server_handle,
            ui_task,
        };
        self.instances.lock().await.insert(id, worker);
        if restore_id.is_none() {
            self.save_registry().await;
        }
        Ok(info)
    }

    /// Stop an instance's tunnels and server without removing it from the list.
    pub async fn stop(&self, id: u64) -> Result<(), String> {
        let map = self.instances.lock().await;
        let inst = map.get(&id).ok_or_else(|| format!("instance {id} not found"))?;
        Self::shutdown_worker(inst).await;
        Ok(())
    }

    /// Stop and remove an instance entirely.
    pub async fn remove(&self, id: u64) -> Result<(), String> {
        let inst = {
            let mut map = self.instances.lock().await;
            map.remove(&id)
                .ok_or_else(|| format!("instance {id} not found"))?
        };
        Self::shutdown_worker(&inst).await;
        self.save_registry().await;
        Ok(())
    }

    /// Stop and drop every instance (used on app shutdown).
    pub async fn shutdown_all(&self) {
        let mut map = self.instances.lock().await;
        for (_, inst) in map.drain() {
            Self::shutdown_worker(&inst).await;
        }
    }

    /// Resolve the path to the persisted instance registry file.
    fn registry_path() -> Result<std::path::PathBuf, String> {
        let base = crate::state::app_config_path().map_err(|e| format!("config path: {e}"))?;
        let dir = base
            .parent()
            .map(|p| p.join("instances"))
            .ok_or_else(|| "could not resolve config directory".to_string())?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("create instances dir: {e}"))?;
        Ok(dir.join("registry.toml"))
    }

    /// Persist the current set of instances so they can be restored on relaunch.
    async fn save_registry(&self) {
        let mut instances: Vec<PersistedInstance> = {
            let map = self.instances.lock().await;
            map.values()
                .map(|w| PersistedInstance {
                    id: w.id,
                    name: w.spec.name.clone(),
                    port: w.spec.port,
                    workspace_root: w.spec.workspace_root.clone(),
                    enable_ngrok: w.spec.enable_ngrok,
                    enable_cloudflare: w.spec.enable_cloudflare,
                    cloudflare_mode: w.spec.cloudflare_mode.clone(),
                    cloudflare_domain: w.spec.cloudflare_domain.clone(),
                    cloudflare_token: w.spec.cloudflare_token.clone(),
                })
                .collect()
        };
        instances.sort_by_key(|p| p.id);
        let file = RegistryFile { instances };
        let path = match Self::registry_path() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("catdesk: failed to resolve registry path: {e}");
                return;
            }
        };
        match toml::to_string_pretty(&file) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    eprintln!("catdesk: failed to write instance registry: {e}");
                }
            }
            Err(e) => eprintln!("catdesk: failed to serialize instance registry: {e}"),
        }
    }

    /// Load the persisted registry, returning an empty one when it is missing.
    fn load_registry() -> Result<RegistryFile, String> {
        let path = Self::registry_path()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("parse registry: {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RegistryFile::default()),
            Err(e) => Err(format!("read registry: {e}")),
        }
    }

    async fn shutdown_worker(inst: &WorkerInstance) {
        {
            let mut app = inst.state.lock().await;
            if let Some(handle) = app.ngrok_task.take() {
                handle.abort();
            }
            if let Some(handle) = app.cloudflare_task.take() {
                handle.abort();
            }
            if let Some(mut child) = app.cloudflare_child.take() {
                let _ = child.start_kill();
            }
            app.ngrok_running = false;
            app.cloudflare_running = false;
            app.server_running = false;
        }
        inst.server_handle.abort();
        inst.ui_task.abort();
    }
}

fn instance_config_path(id: u64) -> Result<std::path::PathBuf, String> {
    let base = crate::state::app_config_path().map_err(|e| format!("config path: {e}"))?;
    let dir = base
        .parent()
        .map(|p| p.join("instances"))
        .ok_or_else(|| "could not resolve config directory".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create instances dir: {e}"))?;
    Ok(dir.join(format!("instance-{id}.toml")))
}

/// Convert an instance name into a URL-safe slug for named-tunnel links.
fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}
