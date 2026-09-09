//! Spawn/gestão do `zcode.cjs app-server` + transporte JSON-RPC.
//!
//! - cwd do filho = pasta do `zcode.cjs` (plano §3.1).
//! - Pipes UTF-8 line-buffered; ids numéricos no cliente.
//! - Requests do servidor respondidos automaticamente (`server_requests`).
//! - Notificações push do filho (método sem id) são repassadas via canal
//!   (`take_event_rx`) para quem quiser (a TUI usa como gatilho de fetch);
//!   sem consumidor, caem no chão em canal finito (sem acumular memória).
//! - `kill_tree()`: Windows usa `taskkill /PID /T /F` (plano §7).

use crate::rpc::{self, RequestId, RpcError};
use crate::server_requests;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("node não encontrado no PATH (node={0}). Instale o Node ou ajuste runtime.node no config.")]
    NodeMissing(String),
    #[error("ZCode não encontrado: {0}")]
    ZcodeMissing(String),
    #[error("falha ao spawnar runtime: {0}")]
    Spawn(String),
    #[error("runtime encerrou (exit={0:?}). Último stderr: {1}")]
    Exited(Option<i32>, String),
    #[error("rpc: {0}")]
    Rpc(#[from] RpcError),
    #[error("timeout após {0}s em {1}")]
    Timeout(u64, String),
    #[error("io: {0}")]
    Io(String),
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

pub struct Runtime {
    child: Mutex<Child>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicU64,
    pid: u32,
    last_stderr: Arc<Mutex<String>>,
    /// Entregue uma única vez via `take_event_rx` (a TUI). O lado emissor
    /// fica com o reader de stdout; canal FINITO: sem consumidor
    /// (headless/REPL) os eventos são descartados, sem acumular memória.
    events_rx: Mutex<Option<mpsc::Receiver<Value>>>,
}

impl Runtime {
    /// Spawna `node zcode.cjs app-server`.
    pub async fn spawn(zcode_cjs: &Path, node_bin: &str) -> Result<Arc<Self>, RuntimeError> {
        if !zcode_cjs.is_file() {
            return Err(RuntimeError::ZcodeMissing(zcode_cjs.to_string_lossy().to_string()));
        }
        let cwd = zcode_cjs.parent().unwrap_or_else(|| Path::new("."));
        let mut cmd = Command::new(node_bin);
        cmd.arg(zcode_cjs)
            .arg("app-server")
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                RuntimeError::NodeMissing(node_bin.to_string())
            } else {
                RuntimeError::Spawn(e.to_string())
            }
        })?;
        let pid = child.id().unwrap_or(0);
        let stdin = child.stdin.take().ok_or_else(|| RuntimeError::Spawn("sem stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| RuntimeError::Spawn("sem stdout".into()))?;
        let stderr = child.stderr.take().ok_or_else(|| RuntimeError::Spawn("sem stderr".into()))?;

        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let last_stderr = Arc::new(Mutex::new(String::new()));
        let (events_tx, events_rx) = mpsc::channel::<Value>(64);

        // Reader stdout (linhas JSON).
        {
            let stdin_w = stdin.clone();
            let pending_r = pending.clone();
            let events_tx_r = events_tx.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    let msg: Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(_) => {
                            tracing::warn!(%line, "linha não-JSON do runtime, ignorada");
                            continue;
                        }
                    };
                    // Request do servidor → responder.
                    if rpc::is_server_request(&msg) {
                        let id = match msg.get("id") {
                            Some(Value::String(s)) => RequestId::Str(s.clone()),
                            _ => continue,
                        };
                        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
                        let params = msg.get("params").cloned().unwrap_or(Value::Null);
                        tracing::debug!(method, "request do servidor");
                        let result = server_requests::answer(method, &params);
                        let resp = rpc::build_server_response(&id, result);
                        let line = rpc::to_line(&resp) + "\n";
                        let mut w = stdin_w.lock().await;
                        if let Err(e) = w.write_all(line.as_bytes()).await {
                            tracing::warn!(%e, "falha ao responder request do servidor");
                        }
                        continue;
                    }
                    // Resposta a request nosso.
                    if rpc::is_client_response(&msg) {
                        if let Some(id) = rpc::response_id(&msg) {
                            let tx = pending_r.lock().await.remove(&id);
                            if let Some(tx) = tx {
                                let out = if msg.get("error").is_some() {
                                    let code = msg["error"].get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
                                    let message = msg["error"].get("message").and_then(|m| m.as_str()).unwrap_or("erro rpc").to_string();
                                    Err(RpcError::Server { code, message })
                                } else if let Some(r) = msg.get("result") {
                                    Ok(r.clone())
                                } else {
                                    Err(RpcError::EmptyResponse(id.to_string()))
                                };
                                let _ = tx.send(out);
                            }
                        }
                        continue;
                    }
                    // Evento/notificação push (stream.chunk, state.updated,
                    // turn finalizado…): repassa ao consumidor da TUI (gatilho
                    // de fetch imediato no poller). Sem consumidor ou com o
                    // canal cheio, descarta — best-effort por design.
                    if msg.get("method").is_some() {
                        tracing::debug!(?msg, "evento push do runtime");
                        let _ = events_tx_r.try_send(msg);
                        continue;
                    }
                    tracing::debug!(?msg, "mensagem desconhecida do runtime");
                }
                tracing::warn!("stdout do runtime fechou (filho pode ter caído)");
            });
        }
        // Drena stderr p/ diagnóstico (últimas linhas).
        {
            let last = last_stderr.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    tracing::debug!(%line, "runtime stderr");
                    let mut g = last.lock().await;
                    g.push_str(&line);
                    g.push('\n');
                    if g.len() > 4000 {
                        let cut = g.len() - 4000;
                        g.drain(..cut);
                    }
                }
            });
        }

        Ok(Arc::new(Self {
            child: Mutex::new(child),
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            pid,
            last_stderr,
            events_rx: Mutex::new(Some(events_rx)),
        }))
    }

    /// Canal de notificações push do filho (métodos sem id). Entregue UMA
    /// única vez — a TUI consome para gatilho de fetch imediato; headless e
    /// REPL nunca chamam e os eventos caem no chão (canal finito).
    pub async fn take_event_rx(&self) -> Option<mpsc::Receiver<Value>> {
        self.events_rx.lock().await.take()
    }

    #[allow(dead_code)]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Chamada RPC com timeout (default 120s p/ turns longos).
    pub async fn call(&self, method: &str, params: Value, timeout_secs: u64) -> Result<Value, RuntimeError> {
        // Checa se o filho morreu antes de chamar (restart explícito fora).
        {
            let mut c = self.child.lock().await;
            match c.try_wait() {
                Ok(Some(status)) => {
                    let tail = self.last_stderr.lock().await.clone();
                    return Err(RuntimeError::Exited(status.code(), tail));
                }
                Ok(None) => {}
                Err(e) => return Err(RuntimeError::Io(e.to_string())),
            }
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = rpc::build_request(id, method, params);
        let line = rpc::to_line(&req) + "\n";
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        {
            let mut w = self.stdin.lock().await;
            w.write_all(line.as_bytes()).await.map_err(|e| RuntimeError::Io(e.to_string()))?;
            w.flush().await.map_err(|e| RuntimeError::Io(e.to_string()))?;
        }
        tracing::debug!(id, method, "rpc →");
        let res = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx)
            .await
            .map_err(|_| RuntimeError::Timeout(timeout_secs, method.to_string()))?
            .map_err(|_| RuntimeError::Rpc(RpcError::Transport("resposta descartada".into())))?;
        res.map_err(RuntimeError::Rpc)
    }

    /// Best-effort: fecha sessão no protocolo (ignora erro).
    pub async fn close_session_best_effort(&self, session_id: &str) {
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.call("session/close", serde_json::json!({ "sessionId": session_id }), 5),
        )
        .await;
    }

    /// Mata a árvore do processo. Windows: taskkill /T /F.
    pub async fn kill_tree(&self) {
        #[cfg(windows)]
        {
            let _ = tokio::process::Command::new("taskkill")
                .args(["/PID", &self.pid.to_string(), "/T", "/F"])
                .output()
                .await;
        }
        #[cfg(not(windows))]
        {
            // kill_on_drop(true) já cuida; tenta kill direto também.
            if let Ok(mut c) = self.child.try_lock() {
                let _ = c.start_kill();
            }
        }
        // Garante wait p/ não virar zumbi.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut c = self.child.lock().await;
            let _ = c.wait().await;
        })
        .await
        .ok();
    }
}
