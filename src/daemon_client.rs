//! Cliente do daemon + transporte unificado (Fase 3, plano V4).
//!
//! - `DaemonClient`: uma conexão TCP com o daemon, MESMA superfície que o
//!   `Runtime` expõe aos fluxos (`call(method, params, timeout)` + canal de
//!   notificações 1× via `take_event_rx`) — os fluxos não sabem a diferença.
//! - `Transport`: `Embedded(Arc<Runtime>)` (comportamento de hoje) ou
//!   `Daemon(DaemonClient)` (sessão quente). O menor-difusão escolhido: o
//!   `Arc<Runtime>` dos fluxos vira `Arc<Transport>`; `Transport::call`
//!   reproduz a assinatura de `Runtime::call`, então o corpo dos fluxos e o
//!   `session.rs` (camada do meio) NÃO mudam de shape — só os tipos.
//! - `decide_transport` (pura): `--no-daemon` → Embedded; entry válida +
//!   connect/handshake no orçamento → Daemon; QUALQUER falha → Embedded.
//!   NUNCA falha o comando por causa do daemon (fallback silencioso).
//! - Auto-start: `daemon.json` ausente/inválida → spawn do próprio exe
//!   (`current_exe() daemon`, detached no Windows, grupo de processo próprio
//!   no Unix) + poll-conecta ≤5s. SINGLE-FLIGHT (I-1b): o spawn é exclusivo
//!   de quem criar `daemon.lock` (data_dir) — N comandos concorrentes geram
//!   no máximo 1 spawn; os perdedores apenas poll-conectam. NÃO acontece no
//!   subcomando `daemon`, em `--stop` nem em `doctor` (eles não passam aqui).
//! - R1 condicional (puro): só tentamos `session/send` pós-resume com
//!   transporte Daemon (sessão possivelmente quente); `-32031` → leitura
//!   com aviso, como hoje. Embutido → nem tenta (idêntico ao atual).

use crate::daemon::{self, ClientMsg, DaemonEntry, DaemonLine};
use crate::config;
use crate::runtime::{Runtime, RuntimeError};
use crate::rpc::RpcError;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};

/// Tentativas do poll de conexão pós auto-start (10 × 500ms = 5s — spec).
pub const AUTOSTART_TRIES: u32 = 10;
pub const AUTOSTART_INTERVAL_MS: u64 = 500;

/// Cliente de uma conexão com o daemon. Conciliável entre tasks: escrita sob
/// lock, respostas multiplexadas por id, notificações em canal próprio.
pub struct DaemonClient {
    next_id: AtomicU64,
    writer: Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RuntimeError>>>>>,
    /// Entregue UMA vez (mesma semântica do `Runtime::take_event_rx`): quem
    /// não consome deixa o canal finito descartar (sem acumular memória).
    notifications: Mutex<Option<mpsc::Receiver<Value>>>,
}

/// Conecta + handshake (hello com token e workspace) dentro do orçamento.
/// Falha em QUALQUER etapa → Err (o chamador cai p/ embutido).
pub async fn connect(
    entry: &DaemonEntry,
    workspace: &str,
    budget: Duration,
) -> Result<DaemonClient, String> {
    tokio::time::timeout(budget, connect_inner(entry, workspace))
        .await
        .map_err(|_| format!("handshake excedeu {}ms", budget.as_millis()))?
}

async fn connect_inner(entry: &DaemonEntry, workspace: &str) -> Result<DaemonClient, String> {
    let stream = TcpStream::connect(("127.0.0.1", entry.port))
        .await
        .map_err(|e| format!("connect 127.0.0.1:{}: {e}", entry.port))?;
    let (reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RuntimeError>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (ntx, nrx) = mpsc::channel::<Value>(64);

    // Reader: respostas por id + notificações p/ o canal (buf: quem não
    // consome deixa transbordar — igual ao canal do runtime).
    {
        let pending_r = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match daemon::parse_daemon_line(&line) {
                    Ok(DaemonLine::Notify { method, params }) => {
                        // Repassa SEM o envelope (mesmo shape que o Runtime
                        // entrega hoje: {"method":..., "params":...}).
                        let _ = ntx.try_send(serde_json::json!({ "method": method, "params": params }));
                    }
                    Ok(DaemonLine::Resp { id, ok, result, error, code }) => {
                        let tx = pending_r.lock().await.remove(&id);
                        if let Some(tx) = tx {
                            let out = if ok {
                                Ok(result)
                            } else {
                                // O `code` opcional do daemon preserva erros
                                // RPC do servidor (R1 depende do -32031).
                                Err(RuntimeError::Rpc(RpcError::Server {
                                    code: code.unwrap_or(-1),
                                    message: error
                                        .unwrap_or_else(|| "erro no daemon".into()),
                                }))
                            };
                            let _ = tx.send(out);
                        }
                    }
                    Err(e) => tracing::debug!(%line, %e, "linha ilegível do daemon, ignorada"),
                }
            }
            // EOF: falha TODOS os pendentes (nada de hang pós-queda).
            let mut p = pending_r.lock().await;
            for (_, tx) in p.drain() {
                let _ = tx.send(Err(RuntimeError::Io("daemon desconectou".into())));
            }
        });
    }

    // Handshake: 1ª linha OBRIGATÓRIA com o token (spec anti-hijack).
    let hello = ClientMsg {
        id: 0,
        token: Some(entry.token.clone()),
        method: "hello".into(),
        params: serde_json::json!({ "workspace": workspace }),
    };
    let (tx0, rx0) = oneshot::channel();
    pending.lock().await.insert(0, tx0);
    write_line(&writer, &daemon::encode_client_msg(&hello))
        .await
        .map_err(|e| format!("enviar hello: {e}"))?;
    match tokio::time::timeout(Duration::from_secs(3), rx0).await {
        Ok(Ok(Ok(_))) => {}
        Ok(Ok(Err(e))) => return Err(format!("handshake recusado: {e}")),
        Ok(Err(_)) | Err(_) => return Err("handshake sem resposta".into()),
    }

    Ok(DaemonClient {
        next_id: AtomicU64::new(1),
        writer,
        pending,
        notifications: Mutex::new(Some(nrx)),
    })
}

async fn write_line(
    writer: &Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    line: &str,
) -> std::io::Result<()> {
    let mut w = writer.lock().await;
    w.write_all(line.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
}

impl DaemonClient {
    /// Request genérico (multiplexado por id) com timeout de envelope —
    /// mesma semântica do `Runtime::call`.
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout_secs: u64,
    ) -> Result<Value, RuntimeError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = ClientMsg { id, token: None, method: method.into(), params };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        write_line(&self.writer, &daemon::encode_client_msg(&msg))
            .await
            .map_err(|e| RuntimeError::Io(e.to_string()))?;
        tracing::debug!(id, method, "daemon rpc →");
        match tokio::time::timeout(Duration::from_secs(timeout_secs), rx).await {
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(RuntimeError::Timeout(timeout_secs, method.to_string()))
            }
            Ok(Err(_)) => Err(RuntimeError::Rpc(RpcError::Transport(
                "resposta descartada".into(),
            ))),
            Ok(Ok(res)) => res,
        }
    }

    /// MESMA assinatura do `Runtime::call` — o daemon repassa ao runtime do
    /// workspace (criado lazy) preservando erros RPC (inclusive -32031).
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        timeout_secs: u64,
    ) -> Result<Value, RuntimeError> {
        self.request(
            "call",
            serde_json::json!({ "rpc": method, "params": params, "timeout_secs": timeout_secs }),
            timeout_secs,
        )
        .await
    }

    /// Ping de latência/liveness (usado por testes e diagnóstico; o doctor
    /// usa `status`).
    #[allow(dead_code)]
    pub async fn ping(&self) -> Result<String, RuntimeError> {
        let v = self.request("ping", serde_json::json!({}), 10).await?;
        Ok(v.as_str().unwrap_or("").to_string())
    }

    pub async fn status(&self) -> Result<Value, RuntimeError> {
        self.request("status", serde_json::json!({}), 10).await
    }

    pub async fn request_shutdown(&self) -> Result<(), RuntimeError> {
        self.request("shutdown", serde_json::json!({}), 10).await?;
        Ok(())
    }

    /// Canal de notificações repassadas pelo daemon (1×, par do runtime).
    pub async fn take_event_rx(&self) -> Option<mpsc::Receiver<Value>> {
        self.notifications.lock().await.take()
    }
}

// ---------- transporte unificado ----------

/// Identificador do transporte (puro, p/ decisões testáveis).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Embedded,
    Daemon,
}

/// Enum com métodos unificados: os fluxos recebem `Arc<Transport>` onde hoje
/// recebem `Arc<Runtime>` (menor-difusão — o `session.rs` só troca o tipo).
pub enum Transport {
    Embedded(Arc<Runtime>),
    Daemon(DaemonClient),
}

impl Transport {
    pub fn kind(&self) -> TransportKind {
        match self {
            Transport::Embedded(_) => TransportKind::Embedded,
            Transport::Daemon(_) => TransportKind::Daemon,
        }
    }

    /// MESMA assinatura do `Runtime::call` — corpos dos fluxos intactos.
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        timeout_secs: u64,
    ) -> Result<Value, RuntimeError> {
        match self {
            Transport::Embedded(rt) => rt.call(method, params, timeout_secs).await,
            Transport::Daemon(c) => c.call(method, params, timeout_secs).await,
        }
    }

    /// Canal de notificações push (1×). Daemon repassa o broadcast local.
    pub async fn take_event_rx(&self) -> Option<mpsc::Receiver<Value>> {
        match self {
            Transport::Embedded(rt) => rt.take_event_rx().await,
            Transport::Daemon(c) => c.take_event_rx().await,
        }
    }

    /// Libera o transporte: embutido mata a árvore do node; daemon apenas
    /// desconecta — a sessão permanece QUENTE no processo residente (payoff
    /// do R1: o próximo `resume`/turno reusa o runtime sem recriar nada).
    pub async fn dispose(&self) {
        match self {
            Transport::Embedded(rt) => rt.kill_tree().await,
            Transport::Daemon(_) => {}
        }
    }
}

// ---------- seleção de transporte (pura) ----------

/// Decisão PURA da seleção (spec Fase 3): `--no-daemon` → Embedded;
/// entry válida + conectável → Daemon; QUALQUER outra combinação → Embedded
/// (fallback GARANTIDO — comando nunca falha por causa do daemon).
pub fn decide_transport(no_daemon: bool, entry_ok: bool, connected: bool) -> TransportKind {
    if no_daemon {
        return TransportKind::Embedded;
    }
    if entry_ok && connected {
        return TransportKind::Daemon;
    }
    TransportKind::Embedded
}

// ---------- R1 condicional (puro) ----------

/// R1: só tentamos `session/send` pós-resume quando a sessão PODE estar
/// quente em UM runtime compartilhado (daemon). Embutido: o resume é
/// leitura garantida (spawna node novo, -32031 sempre no histórico atual) —
/// nem tentamos (comportamento IDÊNTICO a hoje).
pub fn r1_resume_may_send(kind: TransportKind) -> bool {
    matches!(kind, TransportKind::Daemon)
}

/// O erro do `session/send` pós-resume significa "sessão fria NESTE runtime"
/// (-32031)? Aí mantemos a leitura com aviso de hoje. Outros erros também
/// caem p/ leitura, mas com mensagem honesta distinta. Recebe o
/// `SessionError` dos fluxos (o código RPC vive em `Runtime`/`Rpc` dentro dele).
pub fn r1_send_err_means_cold(e: &crate::session::SessionError) -> bool {
    matches!(
        e,
        crate::session::SessionError::Runtime(RuntimeError::Rpc(RpcError::Server {
            code: -32031,
            ..
        }))
    )
}

// ---------- auto-start ----------

/// Spawn do PRÓPRIO exe com arg `daemon`, detached (Windows:
/// CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS via CommandExt — estável;
/// Unix: `process_group(0)` — B-2 da V4, tira o daemon do grupo do CLI p/
/// sinais de terminal (Ctrl+C/SIGHUP do shell não alcançam o daemon).
/// Retorna o pid do processo novo.
pub fn spawn_detached_daemon() -> Result<u32, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // Rust 1.64+ (stable)
    }
    let child = cmd.spawn().map_err(|e| format!("spawn daemon: {e}"))?;
    Ok(child.id())
}

// ----- single-flight do auto-start (I-1b da V4) -----

/// Caminho do `daemon.lock` — MESMO dir do daemon.json (data_dir do CLI).
pub fn daemon_lock_path() -> PathBuf {
    crate::log_file_path()
        .parent()
        .map(|p| p.join("daemon.lock"))
        .unwrap_or_else(|| PathBuf::from("daemon.lock"))
}

/// Guard do lock de single-flight (I-1b): remove o arquivo no Drop — só quem
/// ADQUIRIU o lock possui o guard, então o Drop nunca apaga lock alheio
/// (exceto na race benigna documentada em `acquire_autostart_lock`).
struct AutostartLock {
    path: PathBuf,
}
impl Drop for AutostartLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Adquire o lock EXCLUSIVO do auto-start via `create_new` (átomo do SO):
/// quem CRIA o arquivo ganha o direito de spawnar (→ `Some`); os perdedores
/// (`None`) apenas poll-conectam sem spawnar — N comandos concorrentes geram
/// no máximo 1 spawn de daemon. O dono grava o próprio pid no arquivo: lock
/// com pid VIVO é respeitado; lock stale (dono morto — ex. kill -9, sem
/// cleanup) → remove e tenta `create_new` de novo UMA vez.
///
/// RACE BENIGNA aceita e documentada: (a) leitura do lock recém-criado pode
/// ver arquivo vazio (dono ainda não gravou o pid) e tratá-lo como stale;
/// (b) dois processos podem remover/recriar em janelas sobrepostas. No pior
/// caso DOIS processos spawnam — e o guard SERVER-SIDE (`serve_with`, I-1a)
/// recusa o 2º daemon com erro limpo. Corretude do sistema fica garantida
/// pela camada de baixo; o lock aqui é a otimização que evita o spawn extra.
fn acquire_autostart_lock(path: &Path) -> Option<AutostartLock> {
    let criar = || -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    };
    let grava_pid = |f: std::fs::File| {
        // Best-effort: sem pid gravado, o lock é tratado como stale após a
        // morte do processo (arquivo vazio → parse falha → remove).
        let mut f = f;
        let _ = std::io::Write::write_all(&mut f, std::process::id().to_string().as_bytes());
    };
    match criar() {
        Ok(f) => {
            grava_pid(f);
            Some(AutostartLock { path: path.to_path_buf() })
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Dono vivo (pid gravado e rodando)? → perdemos, só poll-connect.
            if lock_dono_vivo(path) {
                return None;
            }
            // Stale (ilegível, vazio ou pid morto): remove e tenta UMA vez.
            let _ = std::fs::remove_file(path);
            match criar() {
                Ok(f) => {
                    grava_pid(f);
                    Some(AutostartLock { path: path.to_path_buf() })
                }
                Err(_) => None,
            }
        }
        Err(_) => None, // qualquer outro erro de I/O: não somos o spawnista
    }
}

/// O dono do lock em `path` está vivo? pid gravado + `pid_alive`. Arquivo
/// vazio/ilegível → false (tratado como stale pelo chamador — ver doc da
/// race benigna em `acquire_autostart_lock`).
fn lock_dono_vivo(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .map(daemon::pid_alive)
        .unwrap_or(false)
}

/// Auto-start + poll-conecta (≤5s: 10 tentativas de 500ms). Só é chamado
/// quando `daemon.json` está ausente/inválida e NÃO somos o próprio daemon/
/// doctor/--stop. Espera uma entry NOVA (diferente da anterior — evita
/// reconectar numa entrada órfã). Single-flight via `daemon.lock` (I-1b):
/// apenas o processo que adquire o lock spawnou — os demais poll-conectam.
pub async fn autostart_and_connect(
    workspace: &str,
    cfg: &config::FileConfig,
) -> Result<DaemonClient, String> {
    autostart_and_connect_with(
        workspace,
        cfg,
        spawn_detached_daemon,
        daemon_lock_path,
        daemon::daemon_json_path,
    )
    .await
}

/// Núcleo injetável (pureza p/ teste): `spawn`, `lock_path` e `entry_path`
/// são injetados — o spawn REAL rodaria o próprio binário em teste com arg
/// `daemon`, e os caminhos reais tocaria no data_dir do usuário.
async fn autostart_and_connect_with<S, L, E>(
    workspace: &str,
    _cfg: &config::FileConfig,
    spawn: S,
    lock_path: L,
    entry_path: E,
) -> Result<DaemonClient, String>
where
    S: FnOnce() -> Result<u32, String> + Send,
    L: FnOnce() -> PathBuf + Send,
    E: FnOnce() -> PathBuf + Send,
{
    let path = entry_path();
    let anterior = daemon::read_entry(&path);
    // I-1b single-flight: quem cria o `daemon.lock` (create_new atômico) tem
    // o direito exclusivo de spawnar; os perdedores ATRIBUEM a tarefa ao
    // vencedor e apenas poll-conectam. O guard remove o lock ao sair — e o
    // lock vive até o FIM do poll (janela de single-flight = boot inteiro:
    // um comando que chega DURANTE o boot não spawna um 2º daemon).
    let _lock = acquire_autostart_lock(&lock_path());
    if _lock.is_some() {
        spawn()?;
    }
    for _ in 0..AUTOSTART_TRIES {
        tokio::time::sleep(Duration::from_millis(AUTOSTART_INTERVAL_MS)).await;
        if let Some(e) = daemon::read_entry(&path) {
            if anterior.as_ref() == Some(&e) {
                continue; // entry velha ainda em disco
            }
            if let Ok(c) = connect(&e, workspace, Duration::from_millis(daemon::HANDSHAKE_BUDGET_MS))
                .await
            {
                return Ok(c);
            }
        }
    }
    Err("daemon auto-iniciado não respondeu em 5s".into())
}

// ---------- testes ----------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    #[test]
    fn decide_transport_pura_tres_casos_da_spec() {
        assert_eq!(
            decide_transport(true, true, true),
            TransportKind::Embedded,
            "--no-daemon vence tudo → Embedded"
        );
        assert_eq!(
            decide_transport(false, true, true),
            TransportKind::Daemon,
            "entry válida + conectável → Daemon"
        );
        assert_eq!(
            decide_transport(false, true, false),
            TransportKind::Embedded,
            "entry válida + INconectável → Embedded (fallback)"
        );
        assert_eq!(decide_transport(false, false, false), TransportKind::Embedded);
        assert_eq!(decide_transport(false, false, true), TransportKind::Embedded);
    }

    #[test]
    fn r1_condicional_puro_da_spec() {
        // Embedded → nem tenta (comportamento de hoje, zero regressão).
        assert!(!r1_resume_may_send(TransportKind::Embedded));
        // Daemon → tenta send (sessão possivelmente quente).
        assert!(r1_resume_may_send(TransportKind::Daemon));
        // -32031 → sessão fria neste runtime → leitura com aviso.
        let fria = crate::session::SessionError::Runtime(RuntimeError::Rpc(RpcError::Server {
            code: -32031,
            message: "sessão não encontrada neste runtime".into(),
        }));
        assert!(r1_send_err_means_cold(&fria));
        let outra = crate::session::SessionError::Runtime(RuntimeError::Rpc(RpcError::Server {
            code: -32602,
            message: "params".into(),
        }));
        assert!(!r1_send_err_means_cold(&outra));
        assert!(!r1_send_err_means_cold(&crate::session::SessionError::Io("pipe".into())));
    }

    #[tokio::test]
    async fn cliente_contra_daemon_mock_token_call_notificacao_shutdown() {
        // Mock do protocolo em TCP real (sem node): hello+token, 1 call eco,
        // 1 notificação broadcast, shutdown — o CLIENTE de verdade é testado
        // contra ele (spec Fase 3).
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let entry = DaemonEntry {
            port,
            token: "TOK".into(),
            pid: 1,
            started_at: String::new(),
        };
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (r, mut w) = sock.into_split();
            let mut lines = BufReader::new(r).lines();
            // 1) hello com token
            let first = lines.next_line().await.unwrap().unwrap();
            let hello = daemon::auth_line(&first, "TOK").expect("cliente mandou token certo");
            assert_eq!(hello.params["workspace"], "C:/ws");
            w.write_all(daemon::encode_ok(hello.id, serde_json::json!({"hello": true})).as_bytes()).await.unwrap();
            w.write_all(b"\n").await.unwrap();
            // 2) notificação broadcast espontânea
            w.write_all(daemon::encode_notify("session/updated", &serde_json::json!({"n": 1})).as_bytes()).await.unwrap();
            w.write_all(b"\n").await.unwrap();
            // 3) 1 call → eco do rpc
            let line = lines.next_line().await.unwrap().unwrap();
            let msg = daemon::parse_client_line(&line).unwrap();
            assert_eq!(msg.method, "call");
            assert_eq!(msg.params["rpc"], "session/send");
            assert_eq!(msg.params["timeout_secs"], 5);
            w.write_all(daemon::encode_ok(msg.id, serde_json::json!({"echo": msg.params["rpc"]})).as_bytes()).await.unwrap();
            w.write_all(b"\n").await.unwrap();
            // 4) shutdown
            let line = lines.next_line().await.unwrap().unwrap();
            let msg = daemon::parse_client_line(&line).unwrap();
            assert_eq!(msg.method, "shutdown");
            w.write_all(daemon::encode_ok(msg.id, serde_json::json!("shutting down")).as_bytes()).await.unwrap();
            w.write_all(b"\n").await.unwrap();
        });

        let cli = connect(&entry, "C:/ws", Duration::from_secs(2)).await.unwrap();
        // Notificação: MESMA superfície do runtime (take_event_rx 1×).
        let mut rx = cli.take_event_rx().await.expect("canal de notificações");
        let res = cli.call("session/send", serde_json::json!({"x": 1}), 5).await.unwrap();
        assert_eq!(res["echo"], "session/send");
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("notificação não chegou")
            .expect("canal fechou");
        assert_eq!(ev["method"], "session/updated");
        assert_eq!(ev["params"]["n"], 1);
        // take_event_rx é 1× (par do runtime).
        assert!(cli.take_event_rx().await.is_none());
        cli.request_shutdown().await.expect("shutdown ok");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cliente_token_errado_e_conexao_fechada_sem_handshake() {
        // Servidor que lê a 1ª linha e fecha SEM responder (recusa de token —
        // mesmo comportamento do daemon real): handshake do cliente falha,
        // sem pendurar além do orçamento.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (mut r, _) = sock.into_split();
            let mut buf = vec![0u8; 256];
            let _ = r.read(&mut buf).await; // lê o hello e descarta
            // drop do socket → EOF pro cliente
        });
        let entry = DaemonEntry {
            port,
            token: "CERTO".into(),
            pid: 1,
            started_at: String::new(),
        };
        let t0 = std::time::Instant::now();
        assert!(connect(&entry, "C:/x", Duration::from_secs(2)).await.is_err());
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn erro_com_codigo_do_daemon_mapeia_rpc_server() {
        // O `code` opcional da resposta preserva erros RPC: -32031 tem que
        // CHEGAR no RuntimeError do cliente (R1 depende disso).
        let linha = daemon::encode_err(9, "sessão fria", Some(-32031));
        match daemon::parse_daemon_line(&linha).unwrap() {
            DaemonLine::Resp { code, error, .. } => {
                // Mesma conversão que o reader task faz no caminho real.
                let e: RuntimeError = RuntimeError::Rpc(RpcError::Server {
                    code: code.unwrap_or(-1),
                    message: error.unwrap_or_default(),
                });
                let se = crate::session::SessionError::from(e);
                assert!(r1_send_err_means_cold(&se), "-32031 sobrevive ao transporte");
            }
            _ => panic!("esperava resposta"),
        }
    }

    // ----- single-flight do auto-start (I-1b da V4) -----

    /// Pid de um processo JÁ REAPADO (spawn + wait): morto com certeza, sem
    /// chutar pid inexistente. `cmd /C exit` (Windows) / `sh -c exit` (Unix).
    fn pid_morto_para_teste() -> u32 {
        let mut cmd = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" });
        if cfg!(windows) {
            cmd.args(["/C", "exit 0"]);
        } else {
            cmd.args(["-c", "exit 0"]);
        }
        let mut child = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn do processo descartável");
        let pid = child.id();
        child.wait().expect("wait do processo descartável");
        pid
    }

    #[test]
    fn lock_exclusivo_primeiro_ganha_e_drop_remove() {
        // 1º acquire → Some (venceu o create_new); 2º com dono VIVO (o pid
        // gravado é o do próprio processo de teste) → None; Drop do guard
        // remove o arquivo → acquire de novo → Some (higiene do cleanup).
        let dir =
            std::env::temp_dir().join(format!("zc-lock-1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.lock");
        let g1 = acquire_autostart_lock(&path).expect("1º acquire deve ganhar");
        assert!(path.exists());
        assert!(lock_dono_vivo(&path), "pid gravado é o do processo de teste (vivo)");
        assert!(
            acquire_autostart_lock(&path).is_none(),
            "2º acquire com dono vivo → perdedor"
        );
        drop(g1);
        assert!(!path.exists(), "Drop do guard remove o lock");
        assert!(acquire_autostart_lock(&path).is_some(), "lock livre → acquire novamente");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lock_stale_pid_morto_e_assumido() {
        // Lock com pid de processo já reaped → stale → acquire remove e
        // recria (create_new) → Some (somos o novo dono, pid nosso gravado).
        let dir =
            std::env::temp_dir().join(format!("zc-lock-2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.lock");
        std::fs::write(&path, pid_morto_para_teste().to_string()).unwrap();
        let g = acquire_autostart_lock(&path).expect("lock stale deve ser assumido");
        assert!(lock_dono_vivo(&path), "novo dono gravou pid vivo");
        drop(g);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn autostart_2_tasks_concorrentes_geram_1_spawn() {
        // Concorrência REAL com mock: 2 tasks chamam o núcleo injetável
        // (entry/lock em temp dir; spawn grava a entry apontando p/ um mock
        // TCP). Resultado: exatamente 1 spawn, ambos conectam, lock removido.
        let dir =
            std::env::temp_dir().join(format!("zc-lock-3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let entry_path = dir.join("daemon.json");
        let lock_path = dir.join("daemon.lock");

        // Mock do daemon: atende 2 conexões (hello+token → ok) e fecha.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((sock, _)) = listener.accept().await else { return };
                let (mut r, mut w) = sock.into_split();
                let mut line = String::new();
                // hello é a 1ª linha; responde ok (token conferido a seguir).
                if BufReader::new(&mut r).read_line(&mut line).await.is_ok() {
                    let msg = daemon::parse_client_line(&line).unwrap_or(ClientMsg {
                        id: 0,
                        token: None,
                        method: String::new(),
                        params: Value::Null,
                    });
                    if msg.method == "hello" && msg.token.as_deref() == Some("TOK") {
                        let _ = w
                            .write_all(
                                daemon::encode_ok(msg.id, serde_json::json!({"hello": true}))
                                    .as_bytes(),
                            )
                            .await;
                        let _ = w.write_all(b"\n").await;
                    }
                }
                // drop fecha o socket — o cliente já tem o handshake.
            }
        });

        let spawns = Arc::new(AtomicU64::new(0));
        let cfg = config::FileConfig::default();
        let ws = "C:/ws".to_string();

        // "Boot do daemon": a entry aparece 300ms DEPOIS do spawn (como no
        // real, o daemon grava ao subir) — B lê `anterior` = None em t=100ms.
        let entry_boot = entry_path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let e = DaemonEntry {
                port,
                token: "TOK".into(),
                pid: 1,
                started_at: String::new(),
            };
            std::fs::write(&entry_boot, serde_json::to_string(&e).unwrap()).unwrap();
        });

        // Task A (vencedora): adquire o lock e spawna (mock só conta).
        let spawns_a = spawns.clone();
        let lock_a = lock_path.clone();
        let entry_a = entry_path.clone();
        let ws_a = ws.clone();
        let cfg_a = cfg.clone();
        let a = tokio::spawn(async move {
            autostart_and_connect_with(
                &ws_a,
                &cfg_a,
                || {
                    spawns_a.fetch_add(1, Ordering::SeqCst);
                    Ok(999)
                },
                move || lock_a.clone(),
                move || entry_a.clone(),
            )
            .await
        });

        // Janela de 100ms: garante que A adquire o lock ANTES de B (e B lê
        // `anterior` antes da entry existir — a entry nasce em t=300ms).
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Task B (perdedora): lock vivo → apenas poll-conecta.
        let spawns_b = spawns.clone();
        let lock_b = lock_path.clone();
        let entry_b = entry_path.clone();
        let ws2 = ws.clone();
        let cfg2 = cfg.clone();
        let b = tokio::spawn(async move {
            autostart_and_connect_with(
                &ws2,
                &cfg2,
                || {
                    spawns_b.fetch_add(1, Ordering::SeqCst);
                    Ok(999)
                },
                move || lock_b.clone(),
                move || entry_b.clone(),
            )
            .await
        });

        let (ra, rb) = tokio::join!(a, b);
        assert!(matches!(ra.unwrap(), Ok(_)), "A conecta");
        assert!(matches!(rb.unwrap(), Ok(_)), "B conecta (poll sem spawn)");
        assert_eq!(
            spawns.load(Ordering::SeqCst),
            1,
            "2 tasks concorrentes → exatamente 1 spawn"
        );
        assert!(!lock_path.exists(), "lock removido ao sair (guard)");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
