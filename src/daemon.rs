//! Daemon/broker (Fase 3 do plano V4): processo residente dono dos runtimes
//! Node — sessões ficam QUENTES entre comandos (payoff do R1: resume volta a
//! aceitar turno quando a sessão está neste processo).
//!
//! - TCP em 127.0.0.1 porta livre aleatória (`bind("127.0.0.1:0")`).
//! - `daemon.json` no data_dir (mesmo dir do log) com `{port, token, pid,
//!   startedAt}`, escrita ATÔMICA (tmp + rename) e remoção na saída limpa
//!   (shutdown pedido ou Ctrl+C).
//! - Token hex-32B obrigatório na 1ª linha de toda conexão (proteção contra
//!   hijack local por outro processo do mesmo usuário): sem/errado → a
//!   conexão é fechada imediatamente, SEM resposta.
//! - Protocolo newline-JSON (mesmo framing do rpc.rs): request
//!   `{"id":N,"method":"...","params":{...}}` → resposta
//!   `{"id":N,"ok":true,"result":...}` / `{"id":N,"ok":false,"error":"..."
//!   [,"code":-32031]}` (o `code` opcional preserva erros RPC do servidor —
//!   ex.: -32031, usado pelo R1 condicional no cliente).
//! - Notificações do runtime são repassadas a TODOS os clientes conectados
//!   como `{"notification":true,"method":"...","params":...}` (o daemon pega
//!   o `take_event_rx` do runtime UMA vez na criação e faz broadcast).
//! - Runtimes por workspace: `HashMap<workspace, Arc<Runtime>>` criado lazy;
//!   no shutdown (ou Ctrl+C) todos recebem `kill_tree`. Kill NA MARRA do
//!   daemon (kill -9/Task Manager) é coberto pelo Job Object do Windows
//!   (B-1 da V5-3, `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` em runtime.rs) — o
//!   node não fica órfão.
//! - Limite de conexões autenticadas simultâneas (8): quem chega além recebe
//!   erro "busy" e é desconectado — escolha simples e documentada (fila de
//!   espera adicionaria estado; o cliente reabre na próxima operação).
//! - O caminho `call` é despachado em task PRÓPRIA por request: um turno de
//!   minutos não trava ping/status/outros calls da mesma conexão (a TUI faz
//!   calls concorrentes: turno + /usage).

use crate::config;
use crate::runtime::Runtime;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex, Notify, Semaphore};

/// Conexões autenticadas simultâneas; além disso: erro "busy" + close.
pub const MAX_CONNECTIONS: usize = 8;
/// Orçamento total (connect + handshake) do cliente — spec Fase 3.
pub const HANDSHAKE_BUDGET_MS: u64 = 300;

// ---------- framing (puro, testável) ----------

/// Mensagem do cliente p/ o daemon (uma linha JSON). O token só vai na 1ª
/// linha (hello); nas demais é ignorado.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientMsg {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub token: Option<String>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Linha daemon → cliente: resposta (tem id) OU notificação
/// (`notification:true`).
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonLine {
    Resp {
        id: u64,
        ok: bool,
        result: Value,
        error: Option<String>,
        code: Option<i64>,
    },
    Notify { method: String, params: Value },
}

/// Serializa o request do cliente em linha (sem `\n`; par de `parse_client_line`).
pub fn encode_client_msg(m: &ClientMsg) -> String {
    serde_json::to_string(m).unwrap_or_else(|_| "{}".to_string())
}

/// Serializa uma resposta de sucesso.
pub fn encode_ok(id: u64, result: Value) -> String {
    serde_json::json!({ "id": id, "ok": true, "result": result }).to_string()
}

/// Serializa uma resposta de erro (`code` opcional preserva erros RPC).
pub fn encode_err(id: u64, message: &str, code: Option<i64>) -> String {
    let mut v = serde_json::json!({ "id": id, "ok": false, "error": message });
    if let Some(c) = code {
        v["code"] = serde_json::json!(c);
    }
    v.to_string()
}

/// Serializa uma notificação de broadcast.
pub fn encode_notify(method: &str, params: &Value) -> String {
    serde_json::json!({ "notification": true, "method": method, "params": params }).to_string()
}

/// Faz parse de uma linha do cliente (framing newline-JSON).
pub fn parse_client_line(line: &str) -> Result<ClientMsg, String> {
    serde_json::from_str(line.trim()).map_err(|e| format!("linha não é ClientMsg: {e}"))
}

/// Faz parse de uma linha do daemon: resposta (id numérico) ou notificação
/// (`notification:true`).
pub fn parse_daemon_line(line: &str) -> Result<DaemonLine, String> {
    let v: Value =
        serde_json::from_str(line.trim()).map_err(|e| format!("linha não é JSON: {e}"))?;
    if v.get("notification")
        .and_then(|n| n.as_bool())
        .unwrap_or(false)
    {
        let method = v
            .get("method")
            .and_then(|m| m.as_str())
            .ok_or_else(|| "notificação sem method".to_string())?
            .to_string();
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        return Ok(DaemonLine::Notify { method, params });
    }
    Ok(DaemonLine::Resp {
        id: v.get("id").and_then(|i| i.as_u64()).unwrap_or(0),
        ok: v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false),
        result: v.get("result").cloned().unwrap_or(Value::Null),
        error: v
            .get("error")
            .and_then(|e| e.as_str())
            .map(|s| s.to_string()),
        code: v.get("code").and_then(|c| c.as_i64()),
    })
}

/// Falha de autenticação da 1ª linha (a conexão é fechada SEM resposta).
#[derive(Debug, Error, PartialEq)]
pub enum AuthFail {
    #[error("linha de auth não é JSON válido")]
    BadLine,
    #[error("1ª linha não é o handshake 'hello'")]
    NotHello,
    #[error("token ausente ou incorreto")]
    BadToken,
}

/// Valida a 1ª linha da conexão contra o token esperado (puro, testável).
/// Token errado/ausente → `BadToken` (o servidor fecha sem responder —
/// não dá pistas a um processo local hostil).
pub fn auth_line(line: &str, expected_token: &str) -> Result<ClientMsg, AuthFail> {
    let msg = parse_client_line(line).map_err(|_| AuthFail::BadLine)?;
    if msg.method != "hello" {
        return Err(AuthFail::NotHello);
    }
    if msg.token.as_deref() != Some(expected_token) {
        return Err(AuthFail::BadToken);
    }
    Ok(msg)
}

// ---------- daemon.json ----------

/// Entrada de descoberta do daemon (spec: `{port, token, pid, startedAt}`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonEntry {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    #[serde(rename = "startedAt")]
    pub started_at: String,
}

/// Caminho do `daemon.json` — MESMO dir do history/log
/// (`log_file_path().parent()`).
pub fn daemon_json_path() -> PathBuf {
    crate::log_file_path()
        .parent()
        .map(|p| p.join("daemon.json"))
        .unwrap_or_else(|| PathBuf::from("daemon.json"))
}

/// Token hex de 32 bytes (entropia do SO via getrandom).
pub fn generate_token() -> Result<String, String> {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).map_err(|e| format!("entropia indisponível: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Escrita ATÔMICA: bytes primeiro no `*.tmp` (mesmo dir), depois rename —
/// rename sobre destino existente é suportado pelo std no Windows
/// (MoveFileEx REPLACE_EXISTING). Se a escrita do tmp falha, o destino
/// anterior fica INTACTO (o rename nunca chega a rodar).
pub fn write_entry_atomic(path: &Path, entry: &DaemonEntry) -> std::io::Result<()> {
    let bytes = serde_json::to_string_pretty(entry).unwrap_or_else(|_| "{}".into());
    atomic_write(path, bytes.as_bytes())
}

/// Núcleo da escrita atômica (tmp + rename), separado p/ teste de falha.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    write_private(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Grava o tmp com permissão 0600 no Unix (M-1 da V4): o arquivo contém o
/// TOKEN do daemon — o `std::fs::write` padrão cria com 0644, legível por
/// outros usuários locais. `create_new` preserva a semântica atômica (falha
/// se um tmp residual existir). No Windows, escrita normal (o perfil do
/// usuário já restringe a ACL).
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

/// Leitura tolerante: arquivo ausente ou JSON ilegível → None (o chamador
/// trata como "sem daemon" e pode auto-iniciar).
pub fn read_entry(path: &Path) -> Option<DaemonEntry> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Entrada válida? (pura: `is_alive` é INJETADO p/ teste). Porta, pid e
/// token têm que existir; pid vivo confirma que não é entrada órfã.
pub fn daemon_entry_valid(entry: &DaemonEntry, is_alive: bool) -> bool {
    entry.port > 0 && entry.pid > 0 && !entry.token.trim().is_empty() && is_alive
}

/// O pid está vivo? Best-effort por plataforma (Windows: tasklist CSV;
/// Unix: `kill -0`). Qualquer falha → false — o fallback embutido é sempre
/// seguro (falso negativo só perde o daemon; falso positivo é coberto pelo
/// token no handshake).
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new("tasklist");
        cmd.args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]);
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW: o PRÓPRIO daemon (DETACHED, sem console)
            // chama isto no guard de boot — sem o flag, o tasklist ganha
            // console visível novo (janela de terminal abrindo sozinha).
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        match cmd.output() {
            Ok(o) if o.status.success() => {
                // Linha CSV: "nome.exe","PID",... — aspas evitam falso
                // positivo por substring (pid 123 em "1234").
                String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\""))
            }
            _ => false,
        }
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

// ---------- servidor ----------

/// Estado compartilhado do daemon.
struct DaemonState {
    /// Runtimes por workspace, criados LAZY no primeiro `call` do workspace
    /// (sessões ficam quentes no processo — payoff do R1).
    runtimes: Mutex<HashMap<String, Arc<Runtime>>>,
    /// Broadcast das notificações dos runtimes p/ todos os clientes.
    events_tx: broadcast::Sender<Value>,
    /// Orçamento de conexões autenticadas.
    sem: Arc<Semaphore>,
    started: Instant,
    /// Dispara o shutdown limpo (request "shutdown" ou Ctrl+C).
    shutdown: Notify,
    cfg: config::FileConfig,
}

/// Loop principal do daemon (foreground; log no mesmo tracing do CLI).
pub async fn serve(cfg: &config::FileConfig) -> Result<(), String> {
    serve_with(cfg, &daemon_json_path()).await
}

/// Variante com caminho injetado (teste usa temp dir; produção usa data_dir).
pub async fn serve_with(cfg: &config::FileConfig, path: &Path) -> Result<(), String> {
    // Dependências reais de liveness (o teste injeta mocks — ver abaixo).
    let deps = LivenessDeps {
        pid_alive: Box::new(|pid| pid_alive(pid)),
        daemon_responde: Box::new(|entry| Box::pin(daemon_responde(entry))),
    };
    serve_with_deps(cfg, path, &deps).await
}

/// Dependências de liveness INJETÁVEIS do guard de daemon único (I-1a da V4):
/// `pid_alive` decide se a entry em disco pertence a um processo vivo;
/// `daemon_responde` faz o handshake de confirmação na porta. Closures boxed
/// owned: a future de `serve_with_deps` é 'static (spawnável em teste) e o
/// teste injeta mocks sem tocar em código de produção.
struct LivenessDeps {
    pid_alive: Box<dyn Fn(u32) -> bool + Send + Sync>,
    daemon_responde: Box<
        dyn for<'a> Fn(&'a DaemonEntry) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>
            + Send
            + Sync,
    >,
}

/// Handshake de confirmação da entry existente (I-1a): conecta na porta com o
/// token da PRÓPRIA entry (o mesmo caminho do `--stop`) e manda `ping` — pong
/// dentro do orçamento curto (~1s) prova que o daemon está VIVO e respondendo.
/// Qualquer falha (connect, token, timeout) → false — entry considerada stale.
async fn daemon_responde(entry: &DaemonEntry) -> bool {
    let budget = std::time::Duration::from_secs(1);
    tokio::time::timeout(budget, async {
        match crate::daemon_client::connect(entry, "", budget).await {
            Ok(c) => c.ping().await.is_ok(),
            Err(_) => false,
        }
    })
    .await
    .unwrap_or(false)
}

/// Corpo do `serve_with` com liveness injetado (pureza p/ teste do guard).
async fn serve_with_deps(
    cfg: &config::FileConfig,
    path: &Path,
    deps: &LivenessDeps,
) -> Result<(), String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("bind 127.0.0.1:0: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?
        .port();
    let token = generate_token()?;
    // ----- Guard de daemon único (I-1a da V4) -----
    // O bind em :0 sempre passa — sem este guard, o 2º daemon sobrescreveria
    // a entry e o 1º ficaria órfão inalcançável (`--stop` só vê o 2º). Antes
    // de gravar a entry nova: entry existente com pid VIVO que RESPONDE ao
    // handshake → erro limpo, NADA é gravado e nenhum runtime nasce. Pid
    // morto ou handshake falhando → entry stale (caso legítimo) → sobrescreve.
    if let Some(existente) = read_entry(path) {
        let valida = daemon_entry_valid(&existente, (deps.pid_alive)(existente.pid));
        if valida && (deps.daemon_responde)(&existente).await {
            return Err(format!(
                "já existe um daemon vivo (pid {}, porta {}) — use `zcode-cli daemon --stop` antes de iniciar outro",
                existente.pid, existente.port
            ));
        }
        tracing::info!(
            pid = existente.pid,
            porta = existente.port,
            "entry stale encontrada — sobrescrevendo"
        );
    }
    let entry = DaemonEntry {
        port,
        token: token.clone(),
        pid: std::process::id(),
        started_at: chrono::Utc::now().to_rfc3339(),
    };
    write_entry_atomic(path, &entry).map_err(|e| format!("gravar {}: {e}", path.display()))?;
    tracing::info!(port, pid = entry.pid, caminho = %path.display(), "daemon no ar");

    let (events_tx, _) = broadcast::channel(128);
    let state = Arc::new(DaemonState {
        runtimes: Mutex::new(HashMap::new()),
        events_tx,
        sem: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        started: Instant::now(),
        shutdown: Notify::new(),
        cfg: cfg.clone(),
    });

    loop {
        tokio::select! {
            _ = state.shutdown.notified() => {
                tracing::info!("shutdown solicitado por cliente");
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("ctrl+c no daemon — encerrando");
                break;
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((sock, _addr)) => {
                        let st = state.clone();
                        let token2 = token.clone();
                        tokio::spawn(async move {
                            handle_connection(st, sock, token2).await;
                        });
                    }
                    Err(e) => tracing::warn!(%e, "accept falhou (segue no ar)"),
                }
            }
        }
    }

    // ----- limpeza: kill_tree de TODOS os runtimes + remove daemon.json -----
    let runtimes: Vec<Arc<Runtime>> = {
        let mut m = state.runtimes.lock().await;
        std::mem::take(&mut *m).into_values().collect()
    };
    for rt in runtimes {
        rt.kill_tree().await;
    }
    let _ = std::fs::remove_file(path);
    tracing::info!("daemon encerrado (daemon.json removido)");
    Ok(())
}

/// Uma task por conexão: auth na 1ª linha (sem/errado → close imediato, SEM
/// resposta), depois loop de requests + broadcast de notificações.
async fn handle_connection(state: Arc<DaemonState>, sock: TcpStream, token: String) {
    let (reader, writer) = sock.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let mut lines = BufReader::new(reader).lines();

    // 1ª linha = handshake (hello + token). Timeout curto: conexão muda que
    // não fala é descartada.
    let first = match tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
        .await
    {
        Ok(Ok(Some(l))) => l,
        _ => {
            tracing::warn!("conexão fechou antes do handshake");
            return;
        }
    };
    let hello = match auth_line(&first, &token) {
        Ok(m) => m,
        Err(e) => {
            // Sem resposta: um scanner local não aprende nada (o token só
            // está no daemon.json do mesmo usuário).
            tracing::warn!(%e, "handshake recusado — conexão fechada");
            return;
        }
    };
    // Workspace é OPCIONAL no hello: `--stop` e `doctor` conectam só p/
    // ping/status/shutdown. Conexões sem workspace apenas NÃO podem fazer
    // `call` (validado no braço do call, não aqui).
    let workspace = hello
        .params
        .get("workspace")
        .and_then(|w| w.as_str())
        .unwrap_or("")
        .to_string();
    // Limite de conexões: além de 8 autenticadas, erro "busy" + close
    // (documentado no topo do módulo).
    let permit = match state.sem.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            let _ = send_line(
                &writer,
                &encode_err(hello.id, "daemon ocupado (limite de conexões simultâneas)", None),
            )
            .await;
            return;
        }
    };
    if send_line(&writer, &encode_ok(hello.id, serde_json::json!({ "hello": true })))
        .await
        .is_err()
    {
        return;
    }
    tracing::debug!(%workspace, "cliente autenticado");

    // Cada cliente assina o broadcast NO momento em que autentica — eventos
    // anteriores não são entregues (semântica igual ao canal do runtime).
    let mut events = state.events_tx.subscribe();

    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        let fim = handle_request(&state, &workspace, &l, &writer).await;
                        if fim {
                            break;
                        }
                    }
                    _ => break, // EOF/erro: cliente caiu
                }
            }
            ev = events.recv() => {
                match ev {
                    Ok(v) => {
                        let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
                        let params = v.get("params").cloned().unwrap_or(Value::Null);
                        if send_line(&writer, &encode_notify(&method, &params)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Cliente lento: eventos pulados são descartados (o
                        // poll da TUI por intervalo cobre o estado).
                        tracing::warn!(pulados = n, %workspace, "cliente lento — notificações descartadas");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    drop(permit);
    tracing::debug!(%workspace, "cliente desconectado");
}

/// Processa UM request do cliente. Retorna true quando a conexão deve
/// terminar (shutdown pedido).
async fn handle_request(
    state: &Arc<DaemonState>,
    workspace: &str,
    line: &str,
    writer: &Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
) -> bool {
    let msg = match parse_client_line(line) {
        Ok(m) => m,
        Err(e) => {
            let _ = send_line(writer, &encode_err(0, &e, None)).await;
            return false;
        }
    };
    match msg.method.as_str() {
        // O call é despachado em task PRÓPRIA: um turno de minutos não
        // bloqueia ping/status/outros calls da MESMA conexão (multiplexados
        // por id — mesmo modelo do runtime em pipes).
        "call" => {
            if workspace.is_empty() {
                let _ = send_line(
                    writer,
                    &encode_err(msg.id, "hello sem workspace — conexão não faz call", None),
                )
                .await;
                return false;
            }
            let rpc = msg
                .params
                .get("rpc")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
            let params = msg.params.get("params").cloned().unwrap_or(Value::Null);
            let timeout_secs = msg
                .params
                .get("timeout_secs")
                .and_then(|t| t.as_u64())
                .unwrap_or(120);
            let id = msg.id;
            let st = state.clone();
            let ws = workspace.to_string();
            let w = writer.clone();
            tokio::spawn(async move {
                let runtime = match get_runtime(&st, &ws).await {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = send_line(&w, &encode_err(id, &e, None)).await;
                        return;
                    }
                };
                let res = runtime.call(&rpc, params.clone(), timeout_secs).await;
                // AUTO-CURA: se o node deste workspace morreu (p.ex. recebeu um
                // Ctrl+C do console herdado, crash, OOM), o runtime morto NÃO
                // fica preso no mapa servindo o mesmo erro para sempre — o
                // daemon o descarta, recria um node novo e retenta a call UMA
                // vez. Err com node VIVO (erro do servidor) não recria nada.
                let (_runtime, res) = if res.is_err() && runtime.has_exited().await {
                    tracing::warn!(workspace = %ws, "runtime morto detectado — recriando node e retentando a call");
                    st.runtimes.lock().await.remove(&ws);
                    match get_runtime(&st, &ws).await {
                        Ok(fresh) => {
                            let r2 = fresh.call(&rpc, params, timeout_secs).await;
                            (fresh, r2)
                        }
                        Err(e) => (runtime, Err(crate::runtime::RuntimeError::Spawn(e))),
                    }
                } else {
                    (runtime, res)
                };
                let out = match res {
                    Ok(v) => encode_ok(id, v),
                    Err(e) => {
                        // Preserva o código RPC do servidor (ex.: -32031) p/
                        // o R1 condicional no cliente.
                        let (msg_err, code) = match &e {
                            crate::runtime::RuntimeError::Rpc(crate::rpc::RpcError::Server {
                                code,
                                message,
                            }) => (message.clone(), Some(*code)),
                            other => (other.to_string(), None),
                        };
                        encode_err(id, &msg_err, code)
                    }
                };
                let _ = send_line(&w, &out).await;
            });
            false
        }
        "ping" => {
            let _ = send_line(writer, &encode_ok(msg.id, serde_json::json!("pong"))).await;
            false
        }
        "status" => {
            let workspaces: Vec<String> = {
                let m = state.runtimes.lock().await;
                let mut v: Vec<String> = m.keys().cloned().collect();
                v.sort();
                v
            };
            let _ = send_line(
                writer,
                &encode_ok(
                    msg.id,
                    serde_json::json!({
                        "workspaces": workspaces,
                        "uptime_secs": state.started.elapsed().as_secs(),
                        "version": env!("CARGO_PKG_VERSION"),
                    }),
                ),
            )
            .await;
            false
        }
        "shutdown" => {
            let _ =
                send_line(writer, &encode_ok(msg.id, serde_json::json!("shutting down"))).await;
            state.shutdown.notify_one();
            true
        }
        other => {
            let _ = send_line(
                writer,
                &encode_err(msg.id, &format!("método desconhecido: {other}"), None),
            )
            .await;
            false
        }
    }
}

/// Runtime do workspace, criado LAZY. O lock é segurado durante a criação
/// (criação é rara; evita spawn duplicado do mesmo workspace em corrida).
async fn get_runtime(state: &DaemonState, workspace: &str) -> Result<Arc<Runtime>, String> {
    let mut map = state.runtimes.lock().await;
    if let Some(rt) = map.get(workspace) {
        return Ok(rt.clone());
    }
    let zc = config::discover_zcode_cjs(None, &state.cfg).map_err(|e| e.to_string())?;
    let rt = Runtime::spawn(&zc, &state.cfg.runtime.node)
        .await
        .map_err(|e| e.to_string())?;
    // O daemon é o ÚNICO consumidor do canal do runtime (take_event_rx é 1×):
    // re-empacota cada evento no broadcast p/ todos os clientes conectados.
    if let Some(mut rx) = rt.take_event_rx().await {
        let tx = state.events_tx.clone();
        tokio::spawn(async move {
            while let Some(v) = rx.recv().await {
                let _ = tx.send(v); // sem destinatários → erro ignorado (drop)
            }
        });
    }
    tracing::info!(%workspace, pid = rt.pid(), "runtime nasceu no daemon");
    map.insert(workspace.to_string(), rt.clone());
    Ok(rt)
}

/// Escreve UMA linha JSON no socket (com `\n` + flush), sob o lock compartilhado
/// (calls despachados em tasks concorrem com o loop principal da conexão).
async fn send_line(
    writer: &Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    line: &str,
) -> std::io::Result<()> {
    let mut w = writer.lock().await;
    w.write_all(line.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
}

/// Entrada do subcomando `zcode-cli daemon [--stop]`.
pub async fn daemon_main(stop: bool, cfg: &config::FileConfig) -> Result<(), String> {
    if stop {
        return stop_daemon().await;
    }
    serve(cfg).await
}

/// `--stop`: lê daemon.json, conecta (token), manda `shutdown` e confirma.
/// Sem daemon → mensagem honesta e Ok (parar o que não roda é idempotente).
async fn stop_daemon() -> Result<(), String> {
    let path = daemon_json_path();
    let Some(entry) = read_entry(&path) else {
        eprintln!(
            "sem daemon rodando ({} ausente ou ilegível)",
            path.display()
        );
        return Ok(());
    };
    let pid = entry.pid;
    let client = crate::daemon_client::connect(&entry, "", std::time::Duration::from_secs(2))
        .await
        .map_err(|e| format!("daemon (pid {pid}) não respondeu: {e}"))?;
    client
        .request_shutdown()
        .await
        .map_err(|e| format!("shutdown falhou: {e}"))?;
    println!("daemon (pid {pid}) parado.");
    Ok(())
}

// ---------- testes ----------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_req_codifica_e_decodifica() {
        let m = ClientMsg {
            id: 7,
            token: Some("abc".into()),
            method: "call".into(),
            params: serde_json::json!({ "rpc": "session/send", "params": {}, "timeout_secs": 120 }),
        };
        let line = encode_client_msg(&m);
        assert!(!line.contains('\n'), "uma linha = uma mensagem");
        let back = parse_client_line(&line).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn framing_resp_e_notificacao_decodificam() {
        let resp = parse_daemon_line(&encode_ok(3, serde_json::json!({"a": 1}))).unwrap();
        match resp {
            DaemonLine::Resp { id, ok, result, error, code } => {
                assert_eq!((id, ok), (3, true));
                assert_eq!(result["a"], 1);
                assert!(error.is_none() && code.is_none());
            }
            _ => panic!("esperava resposta"),
        }
        // Erro COM código RPC preservado (R1 precisa do -32031).
        let err = parse_daemon_line(&encode_err(4, "sessão fria", Some(-32031))).unwrap();
        match err {
            DaemonLine::Resp { id, ok, error, code, .. } => {
                assert_eq!(id, 4);
                assert!(!ok);
                assert_eq!(error.as_deref(), Some("sessão fria"));
                assert_eq!(code, Some(-32031));
            }
            _ => panic!("esperava resposta de erro"),
        }
        let nota = parse_daemon_line(
            &encode_notify("session/updated", &serde_json::json!({"x": 2})),
        )
        .unwrap();
        match nota {
            DaemonLine::Notify { method, params } => {
                assert_eq!(method, "session/updated");
                assert_eq!(params["x"], 2);
            }
            _ => panic!("esperava notificação"),
        }
        assert!(parse_daemon_line("não json").is_err());
    }

    #[test]
    fn auth_token_errado_rejeitado() {
        let ok_line = encode_client_msg(&ClientMsg {
            id: 0,
            token: Some("t1".into()),
            method: "hello".into(),
            params: serde_json::json!({"workspace": "C:/x"}),
        });
        assert!(auth_line(&ok_line, "t1").is_ok(), "token certo passa");
        let errado = ok_line.replace("\"t1\"", "\"intruso\"");
        assert_eq!(auth_line(&errado, "t1"), Err(AuthFail::BadToken));
        assert_eq!(auth_line(&ok_line, "outro"), Err(AuthFail::BadToken));
        // Sem method hello / linha quebrada.
        assert_eq!(
            auth_line(
                &encode_client_msg(&ClientMsg {
                    id: 0,
                    token: Some("t1".into()),
                    method: "ping".into(),
                    params: Value::Null
                }),
                "t1"
            ),
            Err(AuthFail::NotHello)
        );
        assert_eq!(auth_line("lixo", "t1"), Err(AuthFail::BadLine));
    }

    #[test]
    fn daemon_json_write_read_e_substitui() {
        let dir = std::env::temp_dir().join(format!("zc-daemon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.json");
        assert!(read_entry(&path).is_none(), "ausente → None");
        let e1 = DaemonEntry { port: 1234, token: "aa".into(), pid: 42, started_at: "t1".into() };
        write_entry_atomic(&path, &e1).unwrap();
        assert_eq!(read_entry(&path), Some(e1.clone()));
        // Substituição atômica (rename sobre destino existente).
        let e2 = DaemonEntry { port: 5555, token: "bb".into(), pid: 43, started_at: "t2".into() };
        write_entry_atomic(&path, &e2).unwrap();
        assert_eq!(read_entry(&path), Some(e2));
        // Ilegível → None (leitura tolerante).
        std::fs::write(&path, "{lixo").unwrap();
        assert!(read_entry(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daemon_json_falha_preserva_anterior() {
        // A escrita é tmp PRIMEIRO, rename DEPOIS: tmp em caminho inválido
        // falha antes do rename → o arquivo anterior fica INTACTO.
        let dir = std::env::temp_dir().join(format!("zc-daemon-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.json");
        let e1 = DaemonEntry { port: 1, token: "aa".into(), pid: 1, started_at: "t1".into() };
        write_entry_atomic(&path, &e1).unwrap();
        // Força a falha no tmp: o "dir" pai do alvo é um ARQUIVO.
        let bloqueio = dir.join("bloqueio");
        std::fs::write(&bloqueio, b"x").unwrap();
        let path_quebrada = bloqueio.join("daemon.json");
        let e2 = DaemonEntry { port: 2, token: "bb".into(), pid: 2, started_at: "t2".into() };
        assert!(write_entry_atomic(&path_quebrada, &e2).is_err());
        assert_eq!(read_entry(&path), Some(e1), "entrada anterior intacta");
        // Sanidade: entry malformada é inválida mesmo com pid "vivo".
        assert!(!daemon_entry_valid(
            &DaemonEntry { port: 0, token: "aa".into(), pid: 1, started_at: String::new() },
            true
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entry_validade_por_pid_injetado() {
        let ok = DaemonEntry { port: 9, token: "hex32".into(), pid: 123, started_at: "t".into() };
        assert!(daemon_entry_valid(&ok, true), "tudo presente + pid vivo");
        assert!(!daemon_entry_valid(&ok, false), "pid morto → inválida (stale)");
        assert!(!daemon_entry_valid(
            &DaemonEntry { port: 0, token: "hex32".into(), pid: 123, started_at: "t".into() },
            true
        ));
        assert!(!daemon_entry_valid(
            &DaemonEntry { port: 9, token: "  ".into(), pid: 123, started_at: "t".into() },
            true
        ));
        assert!(!daemon_entry_valid(
            &DaemonEntry { port: 9, token: "hex32".into(), pid: 0, started_at: "t".into() },
            true
        ));
    }

    #[test]
    fn token_gerado_e_hex_32_bytes() {
        let t = generate_token().unwrap();
        assert_eq!(t.len(), 64, "32 bytes = 64 chars hex");
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(generate_token().unwrap(), t, "entropia: dois tokens diferem");
    }

    #[tokio::test]
    async fn daemon_servidor_real_handshake_ping_status_shutdown() {
        // Integração END-TO-END com o SERVIDOR de verdade em TCP real (sem
        // node: `call` precisa de runtime e é coberto pelo mock do cliente).
        // Cobre: escrita do daemon.json, handshake com token, ping, status,
        // recusa de token errado e shutdown com remoção do daemon.json.
        let dir = std::env::temp_dir().join(format!("zc-daemon-srv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.json");
        let cfg = config::FileConfig::default();
        let server = tokio::spawn(async move { serve_with(&cfg, &path).await });

        // Espera o arquivo aparecer e lê a entrada real.
        let mut entry = None;
        for _ in 0..100 {
            if let Some(e) = read_entry(&dir.join("daemon.json")) {
                entry = Some(e);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let entry = entry.expect("daemon não gravou daemon.json");

        // Handshake + ping + status com o CLIENTE de verdade.
        let cli = crate::daemon_client::connect(
            &entry,
            "C:/ws-teste",
            std::time::Duration::from_secs(2),
        )
        .await
        .expect("handshake com o daemon real falhou");
        let pong = cli.ping().await.expect("ping falhou");
        assert_eq!(pong, "pong");
        let st = cli.status().await.expect("status falhou");
        assert_eq!(st["version"], env!("CARGO_PKG_VERSION"));
        assert!(st["workspaces"].as_array().is_some());
        cli.request_shutdown().await.expect("shutdown falhou");

        // Servidor sai limpo (Ok) e REMOVE o daemon.json.
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
        assert!(matches!(r, Ok(Ok(Ok(())))), "serve deve terminar Ok: {r:?}");
        assert!(!dir.join("daemon.json").exists(), "daemon.json removido no shutdown");

        // Cliente NÃO deve pendurar além do orçamento: socket que aceita mas
        // nunca responde ao hello → connect estoura o budget e falha (a
        // recusa de TOKEN em si é coberta por auth_line acima e pelo mock
        // token-errado em daemon_client.rs).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let porta = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let _ = s; // servidor simulado: não responde; fecha no drop
        });
        let mudo = DaemonEntry {
            port: porta,
            token: "qualquer".into(),
            pid: 1,
            started_at: String::new(),
        };
        let t0 = std::time::Instant::now();
        assert!(
            crate::daemon_client::connect(&mudo, "C:/x", std::time::Duration::from_millis(150))
                .await
                .is_err()
        );
        assert!(t0.elapsed() < std::time::Duration::from_secs(2), "respeita o orçamento");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ----- guard de daemon único (I-1a da V4) -----

    /// Espera `daemon.json` ter UMA entry com pid DIFERENTE de `pid_antigo`
    /// (poll curto) e a devolve — cobre o caso de uma entry stale já em disco
    /// que o daemon novo vai sobrescrever.
    async fn espera_entry(dir: &Path, pid_antigo: u32) -> DaemonEntry {
        let path = dir.join("daemon.json");
        for _ in 0..100 {
            if let Some(e) = read_entry(&path) {
                if e.pid != pid_antigo {
                    return e;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("daemon não gravou daemon.json");
    }

    #[tokio::test]
    async fn serve_recusa_segundo_daemon_vivo_e_preserva_entry() {
        // Cenário I-1a REAL: 1º daemon no ar (entry gravada, pid vivo — o
        // próprio processo de teste); 2º serve_with no MESMO caminho deve
        // recusar com erro limpo, sem sobrescrever a entry do 1º.
        let dir = std::env::temp_dir().join(format!("zc-daemon-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.json");
        let cfg = config::FileConfig::default();
        let primeiro = tokio::spawn(async move { serve_with(&cfg, &path).await });
        let entry1 = espera_entry(&dir, 0).await;

        let path2 = dir.join("daemon.json");
        let r = serve_with(&config::FileConfig::default(), &path2).await;
        let msg = r.expect_err("2º daemon deve ser recusado");
        assert!(
            msg.starts_with("já existe um daemon vivo (pid ") && msg.contains("daemon --stop"),
            "erro limpo p/ o usuário: {msg}"
        );
        assert_eq!(
            read_entry(&dir.join("daemon.json")),
            Some(entry1.clone()),
            "entry do 1º daemon intacta (nada foi gravado)"
        );

        // O 1º segue no ar (ping responde) e sai limpo com remoção da entry.
        let cli = crate::daemon_client::connect(
            &entry1,
            "",
            std::time::Duration::from_secs(2),
        )
        .await
        .expect("1º daemon segue alcançável");
        assert_eq!(cli.ping().await.expect("ping falhou"), "pong");
        cli.request_shutdown().await.unwrap();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), primeiro).await;
        assert!(matches!(r, Ok(Ok(Ok(())))), "1º serve termina Ok: {r:?}");
        assert!(!dir.join("daemon.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn serve_recusa_com_mock_respondendo_e_nao_grava_nada() {
        // Injeção pura: pid "vivo" + handshake respondendo (mock) → recusa
        // ANTES de gravar qualquer coisa — a entry em disco fica intacta.
        let deps = LivenessDeps {
            pid_alive: Box::new(|_| true),
            daemon_responde: Box::new(|_| Box::pin(async { true })),
        };
        let dir = std::env::temp_dir().join(format!("zc-daemon-mockv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.json");
        let existente = DaemonEntry {
            port: 1234,
            token: "hex".into(),
            pid: 77,
            started_at: "t".into(),
        };
        write_entry_atomic(&path, &existente).unwrap();
        let msg = serve_with_deps(&config::FileConfig::default(), &path, &deps)
            .await
            .expect_err("mock respondendo → recusa");
        assert!(msg.contains("pid 77") && msg.contains("porta 1234"), "{msg}");
        assert_eq!(read_entry(&path), Some(existente), "entry não sobrescrita");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn serve_pid_morto_ou_handshake_falhando_sobrescreve_stale() {
        // Dois casos legítimos de sobrescrita (entry stale), com injeção:
        // (a) pid morto → nem consulta a porta; (b) pid vivo mas handshake
        // falha → nada respondendo na porta. Em ambos o daemon INICIA e grava
        // a entry nova; o shutdown limpo encerra e remove.
        for (nome, pid_vivo) in [("pid_morto", false), ("handshake_falha", true)] {
            let dir =
                std::env::temp_dir().join(format!("zc-daemon-stale-{nome}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("daemon.json");
            write_entry_atomic(
                &path,
                &DaemonEntry { port: 1, token: "stale".into(), pid: 9, started_at: "t".into() },
            )
            .unwrap();
            let cfg = config::FileConfig::default();
            let server = tokio::spawn(async move {
                let deps = LivenessDeps {
                    pid_alive: Box::new(move |_: u32| pid_vivo),
                    daemon_responde: Box::new(|_: &DaemonEntry| Box::pin(async { false })),
                };
                serve_with_deps(&cfg, &path, &deps).await
            });
            let entry = espera_entry(&dir, 9).await;
            assert_eq!(entry.pid, std::process::id(), "entry nova gravada (sobrescrita)");
            let cli =
                crate::daemon_client::connect(&entry, "", std::time::Duration::from_secs(2))
                    .await
                    .expect("daemon novo responde");
            cli.request_shutdown().await.unwrap();
            let r = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
            assert!(matches!(r, Ok(Ok(Ok(())))), "serve termina Ok: {r:?}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[tokio::test]
    async fn serve_sem_entry_inicia_normalmente() {
        // Sem entry em disco: o guard não interfere (cobre o caminho feliz —
        // o recuso/handshake completos estão nos testes acima e no teste real
        // `daemon_servidor_real_handshake_ping_status_shutdown`).
        let dir = std::env::temp_dir().join(format!("zc-daemon-sem-entry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.json");
        let cfg = config::FileConfig::default();
        let server = tokio::spawn(async move {
            let deps = LivenessDeps {
                pid_alive: Box::new(|_| false),
                daemon_responde: Box::new(|_| Box::pin(async { true })),
            };
            serve_with_deps(&cfg, &path, &deps).await
        });
        let entry = espera_entry(&dir, 0).await;
        let cli = crate::daemon_client::connect(&entry, "", std::time::Duration::from_secs(2))
            .await
            .expect("daemon iniciou sem entry prévia");
        cli.request_shutdown().await.unwrap();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
        assert!(matches!(r, Ok(Ok(Ok(())))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn daemon_json_tmp_com_permissao_0600_no_unix() {
        // M-1 da V4: o daemon.json contém o TOKEN — no Unix o tmp (e o
        // arquivo final após o rename) tem que nascer 0600, não 0644.
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("zc-daemon-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.json");
        write_entry_atomic(
            &path,
            &DaemonEntry { port: 1, token: "segredo".into(), pid: 1, started_at: "t".into() },
        )
        .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "daemon.json deve ser 0600 (mode={mode:o})");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
