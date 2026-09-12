//! Orquestração headless: dispatch de flags/subcomandos, REPL, shutdown gracioso.

use crate::cli::{Cli, Commands};
use crate::config;
use crate::daemon;
use crate::daemon_client::{self, Transport};
use crate::doctor;
use crate::runtime::Runtime;
use crate::session::{self, HistoryEntry};
use crate::ui::tui::{self, ChatMsg, TuiApp, TuiKey};
use crate::ui::{art, input, render, theme};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use serde_json::Value;
use std::io::{BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CmdError {
    #[error("{0}")]
    Runtime(String),
    #[error("{0}")]
    Session(String),
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Io(String),
    /// Turno parado/incompleto (timeout sem resposta, stop). Exit code 2.
    #[error("turno parado: {0}")]
    Stopped(String),
}

/// Exit codes Fase 5: 0 sucesso · 1 erro · 2 turno parado.
pub fn exit_code(e: &CmdError) -> i32 {
    match e {
        CmdError::Stopped(_) => 2,
        _ => 1,
    }
}

/// É erro de turno incompleto (timeout do send_and_wait)? → exit 2.
pub fn is_turn_stalled(e: &CmdError) -> bool {
    match e {
        CmdError::Session(m) => m.contains("timeout"),
        _ => false,
    }
}

impl From<crate::runtime::RuntimeError> for CmdError {
    fn from(e: crate::runtime::RuntimeError) -> Self {
        CmdError::Runtime(e.to_string())
    }
}
impl From<session::SessionError> for CmdError {
    fn from(e: session::SessionError) -> Self {
        CmdError::Session(e.to_string())
    }
}
impl From<config::ConfigError> for CmdError {
    fn from(e: config::ConfigError) -> Self {
        CmdError::Config(e.to_string())
    }
}

/// Emite o aviso de TOML inválido (uma linha em stderr) se houver.
/// Best-effort: nunca falha o comando.
fn emit_config_warning(warning: Option<&str>) {
    if let Some(w) = warning {
        eprintln!("aviso: {w}");
    }
}

/// Load config + aviso de parse no stderr (entrypoint padrão).
fn load_config_warn() -> config::FileConfig {
    let loaded = config::load_config_detailed();
    emit_config_warning(loaded.parse_warning.as_deref());
    loaded.cfg
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub(crate) fn resolve_workspace(cli: &Cli, cfg: &config::FileConfig) -> String {
    let raw = cli
        .cwd
        .clone()
        .unwrap_or_else(|| cfg.default.workspace.clone());
    config::normalize_workspace(&raw)
}

pub(crate) async fn spawn_runtime(
    cli: &Cli,
    cfg: &config::FileConfig,
) -> Result<(Arc<Runtime>, std::path::PathBuf), CmdError> {
    let zc = config::discover_zcode_cjs(cli.runtime.as_deref(), cfg)?;
    // Checa node no PATH com erro claro (plano §7).
    let node_ok = tokio::process::Command::new(&cfg.runtime.node)
        .arg("--version")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !node_ok {
        return Err(CmdError::Config(format!(
            "node não encontrado no PATH (node={}). Instale o Node.js e tente de novo.",
            cfg.runtime.node
        )));
    }
    let rt = Runtime::spawn(&zc, &cfg.runtime.node).await?;
    Ok((rt, zc))
}

/// Runtime EMBUTIDO (comportamento de hoje) embrulhado no transporte.
async fn embedded_transport(cli: &Cli, cfg: &config::FileConfig) -> Result<Arc<Transport>, CmdError> {
    let (rt, _) = spawn_runtime(cli, cfg).await?;
    Ok(Arc::new(Transport::Embedded(rt)))
}

/// Transporte para os fluxos (Fase 3): daemon quando possível, embutido como
/// fallback GARANTIDO — NUNCA falha o comando por causa do daemon.
/// - `--no-daemon` → embutido direto (idêntico a hoje);
/// - `daemon.json` válida (pid vivo) + connect/handshake ≤300ms → Daemon;
/// - entry válida + inconectável → embutido (sem auto-start: o daemon pode
///   estar lento, não vamos spawnar um segundo);
/// - entry ausente/inválida → auto-start (spawn detached + poll ≤5s);
///   falhou → embutido silencioso.
/// NÃO é chamado pelo subcomando `daemon`, `--stop` nem `doctor`.
pub(crate) async fn open_transport(
    cli: &Cli,
    cfg: &config::FileConfig,
    ws: &str,
) -> Result<Arc<Transport>, CmdError> {
    let path = daemon::daemon_json_path();
    let entry = daemon::read_entry(&path);
    let valida = entry
        .clone()
        .filter(|e| daemon::daemon_entry_valid(e, daemon::pid_alive(e.pid)));
    let connected = match &valida {
        Some(e) => daemon_client::connect(
            e,
            ws,
            std::time::Duration::from_millis(daemon::HANDSHAKE_BUDGET_MS),
        )
        .await
        .ok(),
        None => None,
    };
    // Decisão PURA (testável em daemon_client::decide_transport).
    let kind = daemon_client::decide_transport(cli.no_daemon, valida.is_some(), connected.is_some());
    match kind {
        daemon_client::TransportKind::Daemon => {
            tracing::info!(port = valida.as_ref().map(|e| e.port), "usando daemon");
            let c = connected.expect("decide_transport só devolve Daemon com connected");
            Ok(Arc::new(Transport::Daemon(c)))
        }
        daemon_client::TransportKind::Embedded => {
            // Auto-start SOMENTE quando não há entry válida (spec: "ausente/
            // inválida"). Entry válida que não respondeu → embutido direto.
            if !cli.no_daemon && valida.is_none() {
                match daemon_client::autostart_and_connect(ws, cfg).await {
                    Ok(c) => return Ok(Arc::new(Transport::Daemon(c))),
                    Err(e) => tracing::debug!(%e, "auto-start do daemon falhou — embutido"),
                }
            }
            embedded_transport(cli, cfg).await
        }
    }
}

/// Aplica model/mode/thought SOMENTE quando o usuário passou a flag
/// explicitamente. Divergência verificada 2026-09-08: o servidor ao vivo
/// rejeita `zai/glm-5.3` ("Available models: main, zai/glm-5.3-Flash") e o
/// thought corrente é `low` — forçar defaults do config causaria erro/slowdown.
pub(crate) async fn apply_overrides(
    rt: &Arc<Transport>,
    session_id: &str,
    cli: &Cli,
) -> Result<(), CmdError> {
    if let Some(m) = cli.model.as_deref() {
        if !m.trim().is_empty() {
            session::apply_model(rt, session_id, m).await?;
        }
    }
    if let Some(mode) = cli.mode.as_deref() {
        if !mode.trim().is_empty() {
            session::apply_mode(rt, session_id, mode).await?;
        }
    }
    if let Some(th) = cli.thought_level.as_deref() {
        if !th.trim().is_empty() {
            session::apply_thought(rt, session_id, th).await?;
        }
    }
    Ok(())
}

/// Validações puras de UX (testáveis sem runtime).
pub fn validate_resume_id(id: &str) -> Result<(), CmdError> {
    if id.trim().is_empty() {
        return Err(CmdError::Session(
            "resume id inválido: informe o id ou use -c p/ a última da pasta".into(),
        ));
    }
    Ok(())
}

/// Workspace deve existir (após normalização). Erro claro, sem spawn.
pub fn validate_workspace_exists(ws_norm: &str) -> Result<(), CmdError> {
    if ws_norm.trim().is_empty() {
        return Err(CmdError::Config(
            "workspace vazio: informe --cwd <pasta>".into(),
        ));
    }
    if !std::path::Path::new(ws_norm).is_dir() {
        return Err(CmdError::Config(format!(
            "workspace inexistente: {ws_norm} (verifique --cwd / pasta)"
        )));
    }
    Ok(())
}

/// goal set/replace bloqueados no wiring (campo de texto não verificado).
pub fn goal_write_blocked(action: &str) -> bool {
    matches!(action.trim(), "set" | "replace")
}

/// Parse de --allowed-tools/--disallowed-tools: vírgula e/ou espaços.
/// Itens com parênteses (ex.: "Bash(git *)") são mantidos inteiros —
/// a vírgula separa, espaços dentro de parênteses não.
/// Ex.: "Bash(git *), Edit" → ["Bash(git *)", "Edit"].
pub fn parse_tools_filter(raw: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    for seg in raw.unwrap_or("").split([',', '\n']) {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        if seg.contains('(') {
            out.push(seg.to_string());
        } else {
            out.extend(
                seg.split([' ', '\t'])
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string()),
            );
        }
    }
    out
}

/// Aviso honesto Fase 5: filtro parseado mas SEM setter RPC verificado
/// (bundle só mostra settings permission.{allowedTools,disallowedTools}, sem
/// método por sessão). Retorna Some(warning) se algum filtro foi passado.
pub fn tools_filter_warning(allowed: &[String], disallowed: &[String]) -> Option<String> {
    if allowed.is_empty() && disallowed.is_empty() {
        return None;
    }
    Some(
        "filtro de tools ainda não aplicado ao protocolo (sem setter RPC verificado no servidor)"
            .to_string(),
    )
}

/// Nomes de diretórios que NUNCA entram no snapshot (build deps, VCS, caches).
/// Set (não lista) p/ lookup O(1) em workspaces grandes.
fn snapshot_skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".hg"
            | ".svn"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | "out"
            | ".next"
            | ".nuxt"
            | ".turbo"
            | ".cache"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".idea"
            | ".vscode"
            | "vendor"
            | ".cargo"
            | ".gradle"
            | ".terraform"
    ) || name.starts_with('.')
}

/// Snapshot mínimo do workspace: {caminho-relativo → mtime-secs}.
/// Usado p/ diff pós-turno (arquivos criados/modificados).
/// Otimizado p/ árvores grandes: `file_type()` do DirEntry (sem `is_dir()`
/// extra), skip de caches/VCS por nome, e teto anti-travamento.
pub fn snapshot_workspace(ws: &str) -> std::collections::HashMap<String, u64> {
    use std::collections::HashMap;
    let mut out: HashMap<String, u64> = HashMap::new();
    let root = std::path::Path::new(ws);
    let mut stack = vec![root.to_path_buf()];
    // Teto anti-travamento em workspaces gigantes.
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd.flatten() {
            if seen > 20_000 {
                return out;
            }
            seen += 1;
            let ft = match ent.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let name = ent.file_name().to_string_lossy().to_string();
            if ft.is_dir() {
                if snapshot_skip_dir(&name) {
                    continue;
                }
                stack.push(ent.path());
            } else if ft.is_file() {
                if let Ok(md) = ent.metadata() {
                    let p = ent.path();
                    if let Ok(rel) = p.strip_prefix(root) {
                        let mtime = md
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        out.insert(rel.to_string_lossy().replace('\\', "/"), mtime);
                    }
                }
            }
        }
    }
    out
}

#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct WorkspaceDiff {
    pub created: Vec<String>,
    pub modified: Vec<String>,
}

/// Compara snapshots: criados (novo) + modificados (mtime maior).
pub fn diff_snapshots(
    before: &std::collections::HashMap<String, u64>,
    after: &std::collections::HashMap<String, u64>,
) -> WorkspaceDiff {
    let mut created: Vec<String> = after
        .keys()
        .filter(|k| !before.contains_key(*k))
        .cloned()
        .collect();
    let mut modified: Vec<String> = after
        .iter()
        .filter(|(k, m)| before.get(*k).is_some_and(|b| *m > b))
        .map(|(k, _)| k.clone())
        .collect();
    created.sort();
    modified.sort();
    WorkspaceDiff { created, modified }
}

/// `git diff --stat` read-only do workspace (None se não é repo git).
pub fn git_diff_stat(ws: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", ws, "diff", "--stat"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Ação de --notify-on-done (testável): comando | beep | nada.
#[derive(Debug, PartialEq, Eq)]
pub enum NotifyAction {
    None,
    Beep,
    Command(String),
}

pub fn notify_action(on_done: bool, cmd: Option<&str>) -> NotifyAction {
    match (on_done, cmd.map(str::trim).filter(|s| !s.is_empty())) {
        (false, _) => NotifyAction::None,
        (true, Some(c)) => NotifyAction::Command(c.to_string()),
        (true, None) => NotifyAction::Beep,
    }
}

fn run_notify(cli: &Cli) {
    match notify_action(cli.notify_on_done, cli.notify_cmd.as_deref()) {
        NotifyAction::None => {}
        NotifyAction::Beep => eprint!("\x07"),
        NotifyAction::Command(c) => {
            #[cfg(windows)]
            let _ = std::process::Command::new("cmd").args(["/C", &c]).status();
            #[cfg(not(windows))]
            let _ = std::process::Command::new("sh").args(["-c", &c]).status();
        }
    }
}

/// Emite o aviso de tools-filter (stderr, 1 linha) se houver filtros.
fn warn_tools_filter(cli: &Cli) -> Option<String> {
    let a = parse_tools_filter(cli.allowed_tools.as_deref());
    let d = parse_tools_filter(cli.disallowed_tools.as_deref());
    let w = tools_filter_warning(&a, &d)?;
    eprintln!("aviso: {w}");
    Some(w)
}

/// Confirmação y/n (usada pelo /compact, que gasta plano).
pub fn confirm_yes(line: &str) -> bool {
    matches!(
        line.trim().to_lowercase().as_str(),
        "y" | "yes" | "s" | "sim"
    )
}

/// Params do `session/subscribe` p/ a TUI (contrato-fase4-5 §1, Tempo 2).
/// `deliveryKind: "desktop-continuous"` é obrigatório (sem ele → -32602;
/// alternância verificada ao vivo 2026-09-08, Fase 5).
/// NOTA: o núcleo (session.rs) não tem builder de subscribe — por isso o
/// helper vive aqui, sem tocar o núcleo. Streaming incremental real exige
/// expor `stream.chunk` no `runtime.rs` (núcleo, fora do escopo front).
pub fn tui_subscribe_params(session_id: &str) -> Value {
    serde_json::json!({ "sessionId": session_id, "deliveryKind": "desktop-continuous" })
}

/// Tema retro ativo? Opt-in via config; Dark segue default.
pub fn is_retro(cfg: &config::FileConfig) -> bool {
    theme::Theme::from_name(&cfg.ui.theme) == theme::Theme::Retro
}

/// Largura do terminal (fallback 80 fora de tty).
pub fn term_width() -> u16 {
    crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80)
}

fn out_json(cli: &Cli, v: &Value) {
    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".into())
        );
    }
}

/// Shutdown gracioso por transporte: embutido fecha a sessão + kill da
/// árvore; DAEMON apenas desconecta — a sessão permanece QUENTE no processo
/// residente (payoff do R1: o próximo comando reusa o runtime sem spawn).
pub(crate) async fn shutdown(rt: &Arc<Transport>, session_id: Option<&str>) {
    match &**rt {
        Transport::Embedded(inner) => {
            if let Some(s) = session_id {
                inner.close_session_best_effort(s).await;
            }
            inner.kill_tree().await;
        }
        Transport::Daemon(_) => {
            // Sessão quente no daemon: nada a fazer aqui.
        }
    }
}

/// Ctrl+C durante o REPL: tenta `session/stop` (para o turn) e VOLTA ao
/// prompt limpo — não mata o CLI nem o runtime (requisito do handoff front).
/// O loop de stdin bloqueante continua; o sinal só interrompe o turn.
async fn install_ctrlc_hook(rt: Arc<Transport>, session_id: String) {
    let rt2 = rt.clone();
    let sid2 = session_id.clone();
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
            let _ = rt2
                .call("session/stop", session::stop_params(&sid2), 10)
                .await;
            eprintln!("\n(turn parado — prompt limpo. Ctrl+C de novo p/ parar outro turn.)");
        }
    });
}

/// Conteúdo default do config.toml (init).
pub fn default_config_toml(runtime_hint: &str) -> String {
    let zcode = if runtime_hint.is_empty() {
        "# zcode_cjs = \"C:/.../zcode.cjs\"  # opcional; autodetecta se omitido"
    } else {
        &format!("zcode_cjs = \"{}\"", runtime_hint.replace('\\', "\\\\"))
    };
    format!(
        r#"# zcode-cli config — gerado por `zcode-cli init`
# Docs: https://github.com/danjour/CLI-ZCode

[runtime]
{zcode}
node = "node"

[default]
workspace = "."
mode = "build"
# Modelo só é aplicado se --model explícito (servidor Flash em 2026-09).
model = "zai/glm-5.3"
thought_level = "max"

[ui]
theme = "dark"
stream_refresh_ms = 300
"#
    )
}

/// `zcode-cli init`: grava config.toml se ausente (ou --force).
pub fn run_init(force: bool, cli: &Cli) -> Result<(), CmdError> {
    let path = config::config_path();
    if path.exists() && !force {
        return Err(CmdError::Config(format!(
            "config já existe em {} (use --force p/ sobrescrever)",
            path.to_string_lossy()
        )));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CmdError::Io(format!("criar {}: {e}", parent.to_string_lossy())))?;
    }
    let hint = config::discover_zcode_cjs(cli.runtime.as_deref(), &config::FileConfig::default())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let body = default_config_toml(&hint);
    std::fs::write(&path, &body)
        .map_err(|e| CmdError::Io(format!("gravar {}: {e}", path.to_string_lossy())))?;
    if cli.json {
        out_json(
            &cli,
            &serde_json::json!({
                "path": path.to_string_lossy(),
                "wrote": true,
                "runtimeHint": if hint.is_empty() { Value::Null } else { Value::String(hint) },
            }),
        );
    } else {
        println!("config gravado em {}", path.to_string_lossy());
        if !hint.is_empty() {
            println!("runtime detectado: {hint}");
        } else {
            println!("runtime: autodetectará o zcode.cjs no próximo comando");
        }
        println!("próximo: zcode-cli doctor");
    }
    Ok(())
}

/// `zcode-cli completions <shell>` — imprime o script no stdout.
pub fn run_completions(shell: clap_complete::Shell) {
    use clap::CommandFactory;
    let mut cmd = crate::cli::Cli::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
}

/// Lista de sessões p/ o picker headless (server + fallback local).
async fn collect_picker_rows(
    rt: &Arc<Transport>,
    limit: u32,
) -> Result<Vec<session::SessionRow>, CmdError> {
    let res = session::list_sessions(rt, limit).await?;
    let mut rows = session::parse_session_list(&res);
    if rows.is_empty() {
        rows = session::load_history()
            .into_iter()
            .take(limit as usize)
            .map(|e| session::SessionRow {
                id: e.session_id,
                title: if e.title.is_empty() { "-".into() } else { e.title },
                status: "local".into(),
                created: e.updated_at,
            })
            .collect();
    }
    Ok(rows)
}

/// Resolve o id do resume quando não veio na linha de comando.
/// - `-c` → última da pasta
/// - senão + TTY → lista numerada e pede escolha
/// - senão (pipe/CI) → lista e erro com instrução
async fn resolve_resume_id_interactive(
    rt: &Arc<Transport>,
    cli: &Cli,
    ws: &str,
    cont: bool,
) -> Result<String, CmdError> {
    if cont {
        return session::latest_for_workspace(ws)
            .map(|e| e.session_id)
            .ok_or_else(|| {
                CmdError::Session(format!("nenhuma sessão anterior em {ws} (use resume <id>)"))
            });
    }
    let rows = collect_picker_rows(rt, 30).await?;
    if rows.is_empty() {
        return Err(CmdError::Session(
            "nenhuma sessão encontrada (use new <pasta> p/ criar)".into(),
        ));
    }
    // Não-TTY: imprime a lista e ensina o uso — sem prompt interativo.
    if !std::io::stdin().is_terminal() || cli.json {
        let listing = render::format_session_list_numbered(&rows);
        if cli.json {
            let items: Vec<Value> = rows
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    serde_json::json!({
                        "n": i + 1, "id": r.id, "title": r.title,
                        "status": r.status, "created": r.created,
                    })
                })
                .collect();
            out_json(
                &cli,
                &serde_json::json!({
                    "action": "pick",
                    "hint": "escolha com: zcode-cli resume <id>",
                    "sessions": items,
                }),
            );
        } else {
            eprint!("{listing}");
            eprintln!("informe: zcode-cli resume <id>  (ou -c p/ a última da pasta)");
        }
        return Err(CmdError::Session(
            "resume sem id em modo não interativo — escolha um id da lista".into(),
        ));
    }
    // TTY: lista numerada + leitura do número.
    println!("{}", render::format_session_list_numbered(&rows));
    print!("número da sessão (1-{}): ", rows.len());
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| CmdError::Io(format!("ler escolha: {e}")))?;
    let n: usize = line.trim().parse().map_err(|_| {
        CmdError::Session(format!("entrada inválida: {:?}", line.trim()))
    })?;
    if n == 0 || n > rows.len() {
        return Err(CmdError::Session(format!(
            "fora do intervalo 1-{}",
            rows.len()
        )));
    }
    Ok(rows[n - 1].id.clone())
}

pub async fn run(cli: Cli) -> Result<(), CmdError> {
    let cfg = load_config_warn();

    match &cli.command {
        // Subcomando Tui só via main (run_tui); não deve chegar aqui.
        Some(Commands::Tui) => unreachable!("Tui é despachado por run_tui"),
        // doctor: 100% local, zero sessão/gasto (Fase 6).
        Some(Commands::Doctor { fix }) => doctor::run_with_fix(&cli, *fix).await,
        // Escreve config.toml inicial (não sobrescreve sem --force).
        Some(Commands::Init { force }) => run_init(*force, &cli),
        // Shell completions para stdout.
        Some(Commands::Completions { shell }) => {
            run_completions(*shell);
            Ok(())
        }
        // Conselho multi-agente (workers + chefe).
        Some(Commands::Council {
            pergunta,
            agents,
            rounds,
            timeout,
        }) => {
            let ws = resolve_workspace(&cli, &cfg);
            validate_workspace_exists(&ws)?;
            let rt = open_transport(&cli, &cfg, &ws).await?;
            let res = crate::council::run_council(
                &rt,
                &ws,
                pergunta,
                *agents,
                *rounds,
                *timeout,
                cli.json,
            )
            .await;
            shutdown(&rt, None).await;
            res
        }
        // daemon/broker (Fase 3): foreground; --stop pede shutdown limpo.
        // Auto-start NUNCA acontece aqui (o daemon é o próprio processo).
        Some(Commands::Daemon { stop }) => {
            daemon::daemon_main(*stop, &cfg)
                .await
                .map_err(CmdError::Io)?;
            Ok(())
        }
        Some(Commands::Sessions { limit }) => {
            let ws = resolve_workspace(&cli, &cfg);
            let rt = open_transport(&cli, &cfg, &ws).await?;
            let res = session::list_sessions(&rt, *limit).await?;
            if cli.json {
                out_json(&cli, &res);
            } else {
                let t = render::format_sessions_table(&res);
                if t == "(nenhuma sessão)" {
                    let local: Vec<(String, String, String)> = session::load_history()
                        .into_iter()
                        .map(|e| (e.session_id, e.title, e.workspace))
                        .take(*limit as usize)
                        .collect();
                    println!("{}", render::format_local_history(&local));
                } else {
                    print!("{t}");
                }
            }
            shutdown(&rt, None).await;
            Ok(())
        }
        Some(Commands::New { pasta }) => {
            let ws = config::normalize_workspace(pasta);
            validate_workspace_exists(&ws)?;
            warn_tools_filter(&cli);
            let rt = open_transport(&cli, &cfg, &ws).await?;
            let created = session::create_session(&rt, &ws).await?;
            let sid = session::extract_session_id(&created).unwrap_or_default();
            apply_overrides(&rt, &sid, &cli).await.unwrap_or_else(|e| {
                tracing::warn!(%e, "override falhou");
            });
            let title = created
                .get("session")
                .and_then(|s| s.get("title"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let eff = cli
                .model
                .clone()
                .unwrap_or_else(|| session::effective_model(&created));
            session::record_session(HistoryEntry {
                session_id: sid.clone(),
                title,
                workspace: ws.clone(),
                model: eff,
                updated_at: now_rfc3339(),
            });
            if cli.json {
                out_json(
                    &cli,
                    &serde_json::json!({ "sessionId": sid, "workspace": ws }),
                );
            } else {
                println!("sessão criada: {sid} em {ws}");
            }
            shutdown(&rt, Some(&sid)).await;
            Ok(())
        }
        Some(Commands::Fork { id }) => {
            validate_resume_id(id)?;
            let ws = resolve_workspace(&cli, &cfg);
            let rt = open_transport(&cli, &cfg, &ws).await?;
            let res: Value = rt
                .call("session/fork", session::fork_params(id), 60)
                .await
                .map_err(|e| {
                    CmdError::Session(format!("{e}. {}", render::fork_no_checkpoint_hint()))
                })?;
            let new_id = res
                .get("session")
                .and_then(|s| s.get("sessionId"))
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
                .or_else(|| session::extract_session_id(&res))
                .unwrap_or_default();
            if !new_id.is_empty() {
                session::record_session(HistoryEntry {
                    session_id: new_id.clone(),
                    title: String::new(),
                    workspace: resolve_workspace(&cli, &cfg),
                    model: cli.model.clone().unwrap_or_default(),
                    updated_at: now_rfc3339(),
                });
            }
            if cli.json {
                out_json(&cli, &res);
            } else {
                println!("fork: {id} → {new_id}");
            }
            shutdown(&rt, None).await;
            Ok(())
        }
        Some(Commands::Usage { id }) => {
            let ws = resolve_workspace(&cli, &cfg);
            let rt = open_transport(&cli, &cfg, &ws).await?;
            let sid = match id.clone() {
                Some(s) => s,
                None => resolve_usage_target(&cli, &cfg)?,
            };
            let u = session::fetch_usage(&rt, &sid).await?;
            if cli.json {
                out_json(
                    &cli,
                    &serde_json::json!({
                        "sessionId": sid,
                        "totalTokens": u.total_tokens, "inputTokens": u.input_tokens,
                        "outputTokens": u.output_tokens, "reasoningTokens": u.reasoning_tokens,
                        "cacheCreationTokens": u.cache_creation_tokens, "cacheReadTokens": u.cache_read_tokens,
                        "modelRequestCount": u.model_request_count,
                    }),
                );
            } else {
                println!(
                    "usage {sid}: total={} in={} out={} reasoning={} reqs={}",
                    u.total_tokens,
                    u.input_tokens,
                    u.output_tokens,
                    u.reasoning_tokens,
                    u.model_request_count
                );
            }
            shutdown(&rt, None).await;
            Ok(())
        }
        Some(Commands::Resume { id }) => {
            let ws = resolve_workspace(&cli, &cfg);
            let rt = open_transport(&cli, &cfg, &ws).await?;
            let target = match id.clone() {
                Some(s) => {
                    validate_resume_id(&s)?;
                    s
                }
                None => resolve_resume_id_interactive(&rt, &cli, &ws, cli.cont).await?,
            };
            let resumed: Value = rt
                .call("session/resume", session::resume_params(&target), 60)
                .await?;
            let sid = session::extract_session_id(&resumed).unwrap_or_else(|| target.clone());
            // R1 CONDICIONAL (Fase 3): embutido → resume é LEITURA (o node
            // novo nunca teve a sessão; -32031 garantido) — nem tentamos
            // enviar, igual a hoje. Daemon → a sessão PODE estar quente no
            // processo residente: tentamos `session/send`; -32031 (ou outro
            // erro) mantém a leitura + aviso honesto de sempre.
            show_resumed_reading(&rt, &sid).await;
            if let Some(p) = cli.prompt.clone() {
                if daemon_client::r1_resume_may_send(rt.kind()) {
                    match session::send_and_wait(&rt, &sid, &p, 300).await {
                        Ok((_raw, text)) => {
                            if cli.json {
                                out_json(
                                    &cli,
                                    &serde_json::json!({ "sessionId": sid, "response": text }),
                                );
                            } else {
                                println!("{text}");
                            }
                            shutdown(&rt, Some(&sid)).await;
                            return Ok(());
                        }
                        Err(e) => {
                            // Sessão fria neste runtime (-32031) ou outro
                            // erro: nada inventado — leitura + aviso de hoje.
                            if daemon_client::r1_send_err_means_cold(&e) {
                                eprintln!("sessão fria neste runtime ({e}) — seguindo em leitura");
                            } else {
                                eprintln!("envio pós-resume falhou: {e}");
                            }
                        }
                    }
                }
                if cli.json {
                    out_json(
                        &cli,
                        &serde_json::json!({
                            "sessionId": sid, "readOnly": true,
                            "warning": "R1: resume é leitura; novo turno só em sessão warm ou nova. Use: new <pasta> ou zcode-cli sem args.",
                            "ignoredPrompt": p,
                        }),
                    );
                } else {
                    eprintln!(
                        "R1: prompt ignorado (resume é leitura). Use new <pasta> p/ continuar."
                    );
                }
                shutdown(&rt, Some(&sid)).await;
                return Ok(());
            }
            repl_loop_readonly(&rt, &sid, &cli, is_retro(&cfg)).await?;
            shutdown(&rt, Some(&sid)).await;
            Ok(())
        }
        None => {
            // Sem subcomando: one-shot (-p) ou REPL.
            if let Some(p) = cli.prompt.clone() {
                if cli.cont {
                    // -p + -c: R1 = leitura (retomada não aceita novo turno).
                    let ws = resolve_workspace(&cli, &cfg);
                    let target = session::latest_for_workspace(&ws)
                        .map(|e| e.session_id)
                        .ok_or_else(|| {
                            CmdError::Session(format!("nenhuma sessão anterior em {ws}"))
                        })?;
                    validate_resume_id(&target)?;
                    let rt = open_transport(&cli, &cfg, &ws).await?;
                    let resumed: Value = rt
                        .call("session/resume", session::resume_params(&target), 60)
                        .await?;
                    let sid =
                        session::extract_session_id(&resumed).unwrap_or_else(|| target.clone());
                    show_resumed_reading(&rt, &sid).await;
                    // R1 CONDICIONAL: daemon → tenta send (sessão quente);
                    // embutido → nem tenta (idêntico a hoje).
                    if daemon_client::r1_resume_may_send(rt.kind()) {
                        match session::send_and_wait(&rt, &sid, &p, 300).await {
                            Ok((_raw, text)) => {
                                if cli.json {
                                    out_json(
                                        &cli,
                                        &serde_json::json!({ "sessionId": sid, "response": text }),
                                    );
                                } else {
                                    println!("{text}");
                                }
                                shutdown(&rt, Some(&sid)).await;
                                return Ok(());
                            }
                            Err(e) => {
                                if daemon_client::r1_send_err_means_cold(&e) {
                                    eprintln!(
                                        "sessão fria neste runtime ({e}) — seguindo em leitura"
                                    );
                                } else {
                                    eprintln!("envio pós-resume falhou: {e}");
                                }
                            }
                        }
                    }
                    if cli.json {
                        out_json(
                            &cli,
                            &serde_json::json!({
                                "sessionId": sid, "readOnly": true,
                                "warning": "R1: resume é leitura; novo turno só em sessão warm ou nova.",
                                "ignoredPrompt": p,
                            }),
                        );
                    } else {
                        eprintln!(
                            "R1: prompt ignorado (resume é leitura). Use new <pasta> p/ continuar."
                        );
                    }
                    shutdown(&rt, Some(&sid)).await;
                    return Ok(());
                }
                one_shot(&cli, &cfg, &p).await
            } else if cli.cont {
                // -c sem -p: REPL de leitura na última da pasta (R1).
                let ws = resolve_workspace(&cli, &cfg);
                let target = session::latest_for_workspace(&ws)
                    .map(|e| e.session_id)
                    .ok_or_else(|| CmdError::Session(format!("nenhuma sessão anterior em {ws}")))?;
                validate_resume_id(&target)?;
                let rt = open_transport(&cli, &cfg, &ws).await?;
                let resumed: Value = rt
                    .call("session/resume", session::resume_params(&target), 60)
                    .await?;
                let sid = session::extract_session_id(&resumed).unwrap_or_else(|| target.clone());
                show_resumed_reading(&rt, &sid).await;
                repl_loop_readonly(&rt, &sid, &cli, is_retro(&cfg)).await?;
                shutdown(&rt, Some(&sid)).await;
                Ok(())
            } else {
                // REPL novo (warm: envios permitidos no mesmo processo).
                let ws = resolve_workspace(&cli, &cfg);
                validate_workspace_exists(&ws)?;
                let rt = open_transport(&cli, &cfg, &ws).await?;
                let created = session::create_session(&rt, &ws).await?;
                let sid = session::extract_session_id(&created)
                    .ok_or_else(|| CmdError::Session("create não devolveu sessionId".into()))?;
                apply_overrides(&rt, &sid, &cli).await.unwrap_or_else(|e| {
                    tracing::warn!(%e, "override falhou");
                });
                let eff = cli
                    .model
                    .clone()
                    .unwrap_or_else(|| session::effective_model(&created));
                session::record_session(HistoryEntry {
                    session_id: sid.clone(),
                    title: String::new(),
                    workspace: ws.clone(),
                    model: eff,
                    updated_at: now_rfc3339(),
                });
                let rt2 = rt.clone();
                let sid2 = sid.clone();
                install_ctrlc_hook(rt2, sid2).await;
                repl_loop(&rt, &sid, &cli, &created, is_retro(&cfg), &ws).await?;
                shutdown(&rt, Some(&sid)).await;
                Ok(())
            }
        }
    }
}

fn resolve_usage_target(cli: &Cli, cfg: &config::FileConfig) -> Result<String, CmdError> {
    let ws = resolve_workspace(cli, cfg);
    session::latest_for_workspace(&ws)
        .map(|e| e.session_id)
        .ok_or_else(|| {
            CmdError::Session(format!(
                "sem id e sem sessão anterior em {ws}: use usage <id>"
            ))
        })
}

async fn one_shot(cli: &Cli, cfg: &config::FileConfig, prompt: &str) -> Result<(), CmdError> {
    let ws = resolve_workspace(cli, cfg);
    validate_workspace_exists(&ws)?;
    let tools_warn = warn_tools_filter(cli);
    let allowed = parse_tools_filter(cli.allowed_tools.as_deref());
    let disallowed = parse_tools_filter(cli.disallowed_tools.as_deref());
    let snap_before = snapshot_workspace(&ws);
    let rt = open_transport(cli, cfg, &ws).await?;
    let created = session::create_session(&rt, &ws).await?;
    let sid = session::extract_session_id(&created)
        .ok_or_else(|| CmdError::Session("create não devolveu sessionId".into()))?;
    apply_overrides(&rt, &sid, cli).await.unwrap_or_else(|e| {
        tracing::warn!(%e, "override falhou");
    });
    let title = created
        .get("session")
        .and_then(|s| s.get("title"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let eff = cli
        .model
        .clone()
        .unwrap_or_else(|| session::effective_model(&created));
    session::record_session(HistoryEntry {
        session_id: sid.clone(),
        title,
        workspace: ws.clone(),
        model: eff,
        updated_at: now_rfc3339(),
    });
    // @arquivo (Fase V5-1): expansão no início do pipeline do turno — o
    // servidor recebe o prompt com os arquivos embutidos; avisos vão ao
    // stderr (one-shot não tem transcript/statusbar).
    let (prompt_exp, avisos_at) = session::expand_at_files(prompt, &ws);
    for a in &avisos_at {
        eprintln!("{a}");
    }
    let (_raw, text) = session::send_and_wait(&rt, &sid, &prompt_exp, 300)
        .await
        .map_err(|e| {
            let ce = CmdError::from(e);
            if is_turn_stalled(&ce) {
                CmdError::Stopped(ce.to_string())
            } else {
                ce
            }
        })?;
    let u = session::fetch_usage(&rt, &sid).await.unwrap_or_default();
    // Diff Fase 5: git diff --stat se repo, senão snapshot criado/modificado.
    let snap_after = snapshot_workspace(&ws);
    let wd = diff_snapshots(&snap_before, &snap_after);
    let git_stat = git_diff_stat(&ws);
    if cli.json {
        let mut diff_obj = serde_json::json!({ "created": wd.created, "modified": wd.modified });
        if let Some(gs) = git_stat {
            diff_obj["gitStat"] = serde_json::json!(gs);
        }
        let mut out = serde_json::json!({
            "sessionId": sid, "response": text,
            "usage": { "inputTokens": u.input_tokens, "outputTokens": u.output_tokens,
                       "totalTokens": u.total_tokens, "reasoningTokens": u.reasoning_tokens,
                       "modelRequestCount": u.model_request_count },
            "tools": { "allowed": allowed, "disallowed": disallowed },
            "diff": diff_obj,
        });
        if let Some(w) = tools_warn {
            out["tools"]["warning"] = serde_json::json!(w);
        }
        out_json(cli, &out);
    } else {
        println!("{text}");
    }
    run_notify(cli);
    shutdown(&rt, Some(&sid)).await;
    Ok(())
}

/// Relatório completo do pânico gravado em APPEND no log (puro, testável):
/// local + payload. Timestamp e backtrace são acrescentados pelo hook (I/O e
/// não-determinísticos ficam fora da parte pura). Termina em nova linha p/
/// conviver com as linhas do tracing no mesmo arquivo.
pub fn panic_report(payload: &str, loc: &str) -> String {
    format!("panic em {loc}\npayload: {payload}\n")
}

/// Linha curta exibida no stderr depois do terminal restaurado (pura,
/// testável): onde panicanhou, onde está o detalhe e como diagnosticar.
pub fn panic_stderr_line(loc: &str, log: &str) -> String {
    format!("panic em {loc} — detalhes em {log}; rode `zcode-cli doctor`")
}

// ----- gate do panic hook por thread (A-3 da V3; I-2 da V4) -----

// ThreadId da thread dona da UI, num Mutex GLOBAL (OnceLock): o antigo
// thread_local Cell<bool> MENTE sob o scheduler multi-thread do tokio —
// tasks MIGRAM entre worker threads, então a flag valia por THREAD FÍSICA
// e não por tarefa (podia falhar nos dois sentidos). O ThreadId resolve:
// no panic, a thread que executa a task da UI NAQUELE MOMENTO é justamente
// a gravada (a task permanece nela durante todo o unwind) → restaura;
// panic em task secundária → ThreadId diferente → não restaura.
static UI_THREAD_ID: std::sync::OnceLock<std::sync::Mutex<Option<std::thread::ThreadId>>> =
    std::sync::OnceLock::new();

fn ui_thread_slot() -> &'static std::sync::Mutex<Option<std::thread::ThreadId>> {
    UI_THREAD_ID.get_or_init(|| std::sync::Mutex::new(None))
}

/// Esta thread é a dona da UI (alternate screen ativa)? (puro, testável)
pub fn is_ui_thread() -> bool {
    let slot = ui_thread_slot();
    // unwrap_or_else(into_inner): o hook roda durante um panic — mesmo com
    // o Mutex envenenado a consulta tem que funcionar.
    let guard = slot.lock().unwrap_or_else(|p| p.into_inner());
    guard.is_some_and(|id| id == std::thread::current().id())
}

/// Grava o ThreadId da thread atual como dono da UI e devolve um guard que
/// LIMPA no Drop — cobre o fim normal, o caminho de erro (`?`) e o unwind de
/// panic (o hook roda ANTES do unwind: o id ainda está gravado no momento
/// da restauração do terminal, e o guard limpa logo em seguida).
pub fn enter_ui_thread() -> UiThreadGuard {
    let slot = ui_thread_slot();
    let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
    *guard = Some(std::thread::current().id());
    UiThreadGuard
}

/// Guard do ThreadId da UI (ver `enter_ui_thread`).
pub struct UiThreadGuard;
impl Drop for UiThreadGuard {
    fn drop(&mut self) {
        let slot = ui_thread_slot();
        let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
        *guard = None;
    }
}

/// Notice do JoinError do turno (A-3 da V3, pura): a task do turno morreu
/// sem devolver resultado (panic — o abort do Esc não chega aqui). O panic
/// hook já registrou o detalhe no log; a UI destravou `working` e mostra
/// este aviso honesto com o caminho de diagnóstico.
pub fn turn_join_error_notice() -> String {
    "⚠ turno terminou inesperadamente (erro interno — detalhes no log; rode zcode-cli doctor)"
        .to_string()
}

/// Instala o panic hook da TUI (uma vez, no início do `run_tui` — ANTES de
/// qualquer setup). O `TermGuard` cobre o unwind normal do loop, mas:
/// (a) pânico ANTES do guard (setup parcial) deixaria raw mode + alternate
/// screen + mouse capturados ligados; (b) a alternate screen engole a
/// mensagem do pânico. O hook restaura o terminal (mesma ordem do guard),
/// grava o relatório (payload + local + backtrace) em append no log do
/// tracing, imprime a linha curta no stderr e encadeia o hook anterior
/// (`take_hook`) DEPOIS de restaurar — mensagem e exit code intactos.
/// Tudo best-effort: um hook de pânico não pode falhar.
///
/// GATE POR THREAD (A-3 da V3, mecanismo I-2 da V4): a restauração do
/// terminal só acontece se o panic veio da THREAD DA UI (ThreadId gravado
/// pelo `enter_ui_thread` no início do `run_tui` — comparado por id, à prova
/// de migração de task entre workers). Panics em tasks/threads secundárias
/// (poller, turno, ponte de notificações, teclado) NÃO derrubam a alternate
/// screen — seguem logando no arquivo e no stderr como sempre (a UI continua
/// viva).
fn install_panic_hook() {
    use crossterm::{
        event::{DisableBracketedPaste, DisableMouseCapture},
        execute,
        terminal::{disable_raw_mode, LeaveAlternateScreen},
    };
    use std::fs::OpenOptions;
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // 1) Restaura o terminal ANTES de qualquer output (ordem inversa do
        // setup; idempotente com o TermGuard, que roda depois no unwind) —
        // SÓ na thread da UI: tasks secundárias não podem roubar a tela.
        if is_ui_thread() {
            let _ = disable_raw_mode();
            // Bracketed paste SÓ fora do Windows (mesmo gate do setup/TermGuard:
            // o backend Windows do crossterm 0.28 nunca emite `Event::Paste`).
            if !cfg!(windows) {
                let _ = execute!(std::io::stdout(), DisableBracketedPaste);
            }
            let _ = execute!(
                std::io::stdout(),
                DisableMouseCapture,
                LeaveAlternateScreen
            );
        }
        // 2) Relatório completo no log (append). Payload pode ser &str ou
        // String; qualquer outro tipo vira placeholder (não panica no hook).
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(payload não-string)".to_string());
        let loc = info
            .location()
            .map(|l| l.to_string())
            .unwrap_or_else(|| "local desconhecido".to_string());
        let log = crate::log_file_path();
        let relatorio = format!(
            "\n[{}]\n{}backtrace:\n{}\n",
            now_rfc3339(),
            panic_report(&payload, &loc),
            std::backtrace::Backtrace::force_capture()
        );
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&log) {
            let _ = f.write_all(relatorio.as_bytes());
        }
        // 3) Aviso curto no stderr — o terminal já está restaurado, então o
        // usuário VÊ (a mensagem completa estaria atrás da alt screen antes).
        eprintln!("{}", panic_stderr_line(&loc, &log.display().to_string()));
        // 4) Encadeia o hook default: imprime payload/local e preserva o
        // comportamento de exit/abort do runtime.
        prev(info);
    }));
}

/// Slot do runtime compartilhado (Fase V4-1, startup instantâneo):
/// preenchido UMA vez pela task de boot quando o node nasce; poller, turnos e
/// handler de teclas leem daqui (`OnceLock` porque nunca é substituído —
/// Ctrl+N reusa o mesmo runtime, igual ao comportamento anterior).
type RtSlot = std::sync::OnceLock<Arc<Transport>>;

/// Contexto do boot da 1ª sessão: startup instantâneo E retry via Ctrl+N
/// usam a MESMA task. `quit` é setado pela task da UI ao sair do loop — um
/// runtime nascido depois disso é derrubado pela própria task de boot (não
/// fica node órfão).
struct SessionBoot {
    cli: Cli,
    cfg: config::FileConfig,
    rt_slot: Arc<RtSlot>,
    quit: Arc<AtomicBool>,
    /// Gatilho do fetch imediato do poller (compartilhado com o evento-bridge).
    poll_wake: Arc<tokio::sync::Notify>,
}

/// Boot da sessão em background (Fase V4-1): runtime + `session/create` +
/// `apply_overrides` + `record_session` + `subscribe` — exatamente o fluxo
/// que ANTES rodava antes do EnterAlternateScreen. O resultado volta pela
/// fila como `UiUpdate::NewSession` (o mesmo caminho do Ctrl+N: o apply
/// troca o sid do watch, limpa `runtime_dead`, marca `session_ready` e
/// reseta o snapshot do poller). Falhas são degradação honesta: notice com
/// retry via Ctrl+N; `Exited` vira o banner de runtime morto (mapeamento
/// existente — sem runtime_dead automático para RPC/protocolo).
fn spawn_session_boot(boot: &SessionBoot, ui_tx: &UiTx, ws: &str) {
    let cli = boot.cli.clone();
    let cfg = boot.cfg.clone();
    let rt_slot = boot.rt_slot.clone();
    let quit = boot.quit.clone();
    let poll_wake = boot.poll_wake.clone();
    let tx = ui_tx.clone();
    let ws2 = ws.to_string();
    tokio::spawn(async move {
        // 1) Transporte (Fase 3): reusa o do slot (retry pós-create-falha,
        // Ctrl+N) ou abre um agora (1ª vez) — daemon com fallback embutido.
        let rt = match rt_slot.get() {
            Some(rt) => rt.clone(),
            None => match open_transport(&cli, &cfg, &ws2).await {
                Ok(rt) => {
                    // UI saiu enquanto o transporte nascia: embutido derruba
                    // o filho AGORA (daemon: só desconecta — sessão quente).
                    if quit.load(Ordering::SeqCst) {
                        rt.dispose().await;
                        return;
                    }
                    let _ = rt_slot.set(rt.clone());
                    // Ponte de notificações push → poller/UI (uma única vez:
                    // `take_event_rx` drena o receptor — runtime OU daemon).
                    if let Some(mut ev_rx) = rt.take_event_rx().await {
                        let tx3 = tx.clone();
                        let wake3 = poll_wake.clone();
                        tokio::spawn(async move {
                            while let Some(msg) = ev_rx.recv().await {
                                wake3.notify_one();
                                let method = msg
                                    .get("method")
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if tx3.send(UiUpdate::SessionEvent { method }).is_err() {
                                    break;
                                }
                            }
                        });
                    }
                    rt
                }
                Err(e) => {
                    let _ = tx.send(UiUpdate::Notice(render::session_boot_fail_notice(
                        &e.to_string(),
                    )));
                    return;
                }
            },
        };
        // 2) Sessão (o resto é o fluxo antigo do run_tui, pós-create).
        let created = match session::create_session(&rt, &ws2).await {
            Ok(c) => c,
            Err(e) => {
                let upd = match runtime_exited_info(&e) {
                    Some((code, tail)) => UiUpdate::RuntimeDead {
                        notice: render::runtime_dead_banner(code, &tail),
                    },
                    None => UiUpdate::Notice(render::session_boot_fail_notice(&e.to_string())),
                };
                let _ = tx.send(upd);
                return;
            }
        };
        let sid = session::extract_session_id(&created).unwrap_or_default();
        if sid.is_empty() {
            let _ = tx.send(UiUpdate::Notice(render::session_boot_fail_notice(
                "create não devolveu sessionId",
            )));
            return;
        }
        apply_overrides(&rt, &sid, &cli).await.unwrap_or_else(|e| {
            tracing::warn!(%e, "override falhou");
        });
        let eff = cli
            .model
            .clone()
            .unwrap_or_else(|| session::effective_model(&created));
        session::record_session(HistoryEntry {
            session_id: sid.clone(),
            title: String::new(),
            workspace: ws2.clone(),
            model: eff.clone(),
            updated_at: now_rfc3339(),
        });
        // Push stream (best-effort): as notificações agora são ENTREGUES ao
        // loop (canal do runtime + gatilho de fetch no poller); o poll por
        // intervalo continua como fallback se nenhuma chegar.
        if let Err(e) = rt
            .call("session/subscribe", tui_subscribe_params(&sid), 15)
            .await
        {
            tracing::warn!(%e, "subscribe falhou (poll mantido)");
        }
        let _ = tx.send(UiUpdate::NewSession {
            created,
            sid,
            model: eff,
        });
    });
}

/// Critério puro da guarda de TTY (testável sem terminal real — injete os
/// flags): a TUI exige stdin E stdout em terminal. Por quê AMBOS:
/// - stdout em arquivo/pipe: contexto não interativo — a TUI desenharia ANSI
///   num destino que não é tela;
/// - stdin fora de TTY (`< /dev/null`): sem teclado — no Unix o leitor de
///   eventos leria EOF em loop.
/// Evidência empírica (Windows 11, Git Bash, console anexado; `IsTerminal`
/// do std consulta `GetConsoleMode` no handle real): com stdout redirecionado
/// a arquivo ou pipe, `stdout().is_terminal()` = false; com stdin em
/// `/dev/null`, stdin = false. O "fantasma" comprovado (`zcode-cli tui
/// < /dev/null > arquivo` pendurava: sessão invisível criada, frames
/// desenhados no arquivo) cai nos dois flags — o E cobre qualquer combinação
/// de redirect. O crossterm pendurava porque abre CONIN$/CONOUT$ próprios,
/// sem olhar stdin/stdout; por isso a TUI vivia invisível.
pub(crate) fn ensure_tui_tty(stdin_tty: bool, stdout_tty: bool) -> Result<(), CmdError> {
    if stdin_tty && stdout_tty {
        Ok(())
    } else {
        Err(CmdError::Io(
            "a TUI requer um terminal interativo (stdin/stdout em TTY). Para saída não interativa use -p/--json.".into(),
        ))
    }
}

/// TUI rica (Fase 4, Tempo 1): alternate screen ratatui, input multilinha,
/// atalhos e componente de permissão stub. Sai matando o filho (shutdown).
/// Sem terminal interativo no ambiente: só código + fixtures aqui.
///
/// Arquitetura de fluidez (Fase 7): a task da UI NUNCA faz rede. Um poller
/// dedicado busca `session/messages` (cadence curto `working`, longo ocioso,
/// gatilho imediato por notificação do runtime) e publica `UiUpdate` num
/// canal; RPCs de tecla (Enter/Ctrl+N/Ctrl+U/Esc/slash) vão para
/// `tokio::spawn` e devolvem pelo mesmo canal; o loop desenha só quando
/// "dirty" (teclado/mouse, qualquer `UiUpdate`, ou tick de animação se
/// `working`), drenando os lotes antes de um único draw.
pub async fn run_tui(cli: Cli) -> Result<(), CmdError> {
    use crate::ui::{render, theme::Theme, tui};
    use crossterm::{
        event::{
            self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
            EnableMouseCapture, Event,
        },
        execute,
        terminal::{
            disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
        },
    };
    use ratatui::{backend::CrosstermBackend, Terminal};

    // Panic hook ANTES de qualquer setup (inclusive o parcial): um pânico
    // daqui p/ frente restaura o terminal e registra o relatório no log —
    // mesmo que ocorra antes do TermGuard existir.
    install_panic_hook();
    // A-3 (V3): marca ESTA thread como a dona da UI — o hook acima só
    // restaura o terminal para panics daqui (tasks secundárias apenas
    // logam). O guard limpa a flag na saída, inclusive em erro/`?`.
    let _ui_thread = enter_ui_thread();

    // Guarda de TTY ANTES de qualquer setup (raw mode/alternate screen) e
    // antes de resolver workspace/sessão: sem TTY não nasce runtime nem
    // sessão — erro claro na stderr e exit 1 (critério/documentação em
    // `ensure_tui_tty`).
    ensure_tui_tty(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    )?;
    // Observabilidade (1 linha por execução da TUI, no log de arquivo):
    // identifica o processo no diagnóstico do "fantasma" (pid + versão).
    let pid = std::process::id();
    tracing::info!(pid, versao = env!("CARGO_PKG_VERSION"), "TUI iniciando");

    let cfg = load_config_warn();
    let ws = resolve_workspace(&cli, &cfg);
    validate_workspace_exists(&ws)?;

    struct TermGuard;
    impl Drop for TermGuard {
        fn drop(&mut self) {
            // Ordem inversa do setup: desliga o bracketed paste antes de sair
            // da tela alternativa (best-effort — sem pânico no drop).
            let _ = disable_raw_mode();
            // Bracketed paste SÓ fora do Windows (gate espelho do setup): no
            // crossterm 0.28 o backend Windows de eventos NUNCA emite
            // `Event::Paste` (parser de paste só existe em
            // `event/sys/unix/parse.rs`; em `event/sys/windows/parse.rs` cada
            // char VT vira `KeyEvent` individual), então os marcadores
            // `?2004h/l` entregues pelo terminal viram LIXO no input
            // (`Esc` + `Char('[')`… — e o `Esc` dispara stop de turno).
            if !cfg!(windows) {
                let _ = execute!(std::io::stdout(), DisableBracketedPaste);
            }
            let _ = execute!(
                std::io::stdout(),
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            // Identidade: devolve o título padrão da janela — o título da TUI
            // não deve vazar para o shell depois que a interface sai.
            let _ = execute!(std::io::stdout(), SetTitle("zcode-cli"));
        }
    }

    enable_raw_mode().map_err(|e| CmdError::Io(format!("raw mode: {e}")))?;
    // Bracketed paste: colagens chegam como UM `Event::Paste` (texto inteiro
    // no buffer, sem enviar linha a linha) — caminho correto em Unix/WSL, onde
    // o parser VT (`event/sys/unix/parse.rs`) emite `Event::Paste`.
    // NO Windows nativo é REGRESSÃO habilitar: `?2004h` É enviado via ANSI
    // (o suporte winapi é `Err(Unsupported)`; `event.rs` do crossterm 0.28),
    // mas o parser Windows (`event/sys/windows/parse.rs`) nunca emite
    // `Event::Paste` — cada char do marcador viraria tecla (`Esc` + `[200~`),
    // e o `Esc` dispararia stop de turno. Nesses builds o paste multilinha é
    // detectado por timing na thread de teclado (logo abaixo).
    execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture)
        .map_err(|e| CmdError::Io(format!("alternate screen: {e}")))?;
    // Identidade: a janela se identifica ("ZCode TUI") em vez de herdar o
    // título do shell. Best-effort — título é cosmético, nunca derruba a TUI
    // (o TermGuard restaura o título padrão na saída).
    let _ = execute!(std::io::stdout(), SetTitle("ZCode TUI"));
    if !cfg!(windows) {
        execute!(std::io::stdout(), EnableBracketedPaste)
            .map_err(|e| CmdError::Io(format!("bracketed paste: {e}")))?;
    }
    let _guard = TermGuard;
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))
        .map_err(|e| CmdError::Io(format!("terminal: {e}")))?;

    let theme = Theme::from_name(&cfg.ui.theme);
    let pal = if crate::ui::theme::detect_256() {
        theme.palette_256()
    } else {
        theme.palette()
    };

    // Fase V4-1 (startup instantâneo): a TUI existe ANTES da sessão — sid no
    // placeholder "", status "conectando…" e Enter bloqueado (gate
    // `session_ready` no handler) até o `UiUpdate::NewSession` do boot. A
    // splash já renderiza (a arte não depende de sessão); `created` também é
    // placeholder (o /model só lê dele depois do boot).
    let mut created: Value = serde_json::json!({});
    let mut app = TuiApp::new("", &ws, false);
    app.status = "conectando…".to_string();
    app.mode = cli.mode.clone().unwrap_or_else(|| cfg.default.mode.clone());
    app.push_msg("system", &render::perm_unsupported_note());
    // Splash = mensagem-marcador (role "system"); o conteúdo visual (arte
    // half-block truecolor) é escolhido por tui::render a partir da área
    // histórica atual, inclusive após resize.
    app.push_msg("system", render::splash(60));

    // Teclado em thread bloqueante → canal (sem dep futures p/ EventStream).
    // Detector de paste por timing: no Windows nativo o crossterm 0.28 nunca
    // emite `Event::Paste` real (ver gate acima) — a colagem multilinha chega
    // como `Event::Key` velozes e o `Enter` do meio do texto ENVIARIA a 1ª
    // linha. Enter SIMPLES <20ms desde a tecla anterior é paste sintetizado →
    // vira `Event::Paste("\n")` (o handler insere no buffer em vez de enviar).
    // Em Unix é no-op: a colagem chega como UM `Event::Paste`, que passa direto
    // (nunca como sequência de Keys).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || {
        let mut last_key_at: Option<std::time::Instant> = None;
        loop {
            match event::read() {
                Ok(ev) => {
                    // Só `Event::Key` mexe no relógio (Mouse/Resize não "colam"
                    // e não redefinem a janela de timing).
                    let ev = if matches!(ev, Event::Key(_)) {
                        let convertido = enter_to_newline(last_key_at, &ev);
                        // Atualiza ANTES de despachar a decisão, para TODO Key
                        // (incluindo o convertido): o 2º `Enter` de uma linha
                        // em branco colada também chega <20ms e converte.
                        last_key_at = Some(std::time::Instant::now());
                        convertido.unwrap_or(ev)
                    } else {
                        ev
                    };
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // ----- canais da Fase 7 -----
    // ui_tx/ui_rx: única fronteira entre as tasks auxiliares e a task da UI
    // (dona exclusiva do TuiApp). working_tx: cadence do poller. sid_tx:
    // sessão ativa p/ o poller (Ctrl+N) — começa no PLACEHOLDER "" (gate do
    // poller até a 1ª sessão existir). poll_wake: notificação → fetch
    // imediato, sem esperar o intervalo.
    let (ui_tx, mut ui_rx): (UiTx, UiRx) = tokio::sync::mpsc::unbounded_channel();
    let (working_tx, working_rx) = tokio::sync::watch::channel(false);
    let (sid_tx, sid_rx) = tokio::sync::watch::channel(String::new());
    let poll_wake = std::sync::Arc::new(tokio::sync::Notify::new());

    // ----- boot da sessão em background (Fase V4-1) -----
    // Runtime no slot compartilhado (OnceLock: escrito 1× quando o node
    // nasce; poller/turnos/handler leem daqui); `quit` avisa a task quando a
    // UI sai do loop (um filho nascido tarde é derrubado lá dentro). A ponte
    // de notificações push nasce JUNTO do runtime, dentro da task.
    let rt_slot: Arc<RtSlot> = Arc::new(std::sync::OnceLock::new());
    let quit_flag = Arc::new(AtomicBool::new(false));
    let boot = SessionBoot {
        cli: cli.clone(),
        cfg: cfg.clone(),
        rt_slot: rt_slot.clone(),
        quit: quit_flag.clone(),
        poll_wake: poll_wake.clone(),
    };
    spawn_session_boot(&boot, &ui_tx, &ws);
    // B-3 da V4: o boot inicial está em voo — Ctrl+N/Enter repetidos durante
    // o "conectando…" são ignorados (single-flight) até a resposta chegar.
    app.booting = true;

    // Poller dedicado (A): rede fora do braço de teclado. Termina quando a
    // task da UI sai (o receptor do canal cai → send falha → break).
    {
        let rt_slot2 = rt_slot.clone();
        let ui_tx2 = ui_tx.clone();
        let mut working_rx2 = working_rx.clone();
        let mut sid_rx2 = sid_rx.clone();
        let wake2 = poll_wake.clone();
        let cfg_ms = cfg.ui.stream_refresh_ms;
        tokio::spawn(async move {
            let mut poll_sid = sid_rx2.borrow_and_update().clone();
            let mut snapshot: Vec<session::MsgItem> = Vec::new();
            let mut fail_streak: u32 = 0;
            // Backoff (falhas CONSECUTIVAS de fetch): contador PRÓPRIO — conta
            // TODA falha real (protocolo/RPC incluso: servidor vivo em apuros
            // também deve ser pollado mais devagar), enquanto o `fail_streak`
            // de morte continua contando só Exited/Io. Timeout de envelope não
            // mexe aqui (não é falha de fato); sucesso zera.
            let mut backoff_fails: u32 = 0;
            let mut dead_notified = false;
            loop {
                let working = *working_rx2.borrow_and_update();
                // Em falha consecutiva, a próxima espera cresce exponencialmente
                // (base × 2^fails, teto 15s). Uma notificação do runtime no
                // `select!` INTERROMPE o backoff: fetch imediato é sinal de vida.
                let ms = poll_backoff_ms(poll_interval_ms(working, cfg_ms), backoff_fails);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(ms)) => {}
                    _ = wake2.notified() => {} // notificação → fetch imediato (corta o backoff)
                    r = sid_rx2.changed() => {
                        if r.is_err() {
                            break; // UI saiu (sid_tx dropado) — encerra junto
                        }
                    } // troca de sessão → refetch
                }
                let sid_now = sid_rx2.borrow_and_update().clone();
                if sid_now != poll_sid {
                    snapshot.clear();
                    poll_sid = sid_now;
                }
                // Fase V4-1 (startup instantâneo): gates do boot — runtime do
                // slot (None = spawn em andamento) e sid REAL (o placeholder
                // "" só sai com o 1º NewSession). Sem fetch: volta ao select
                // (a cadência segue; `sid_rx.changed()` acorda na hora quando
                // a sessão nasce — o gate não atrasa o 1º poll).
                let rt2 = match rt_slot2.get() {
                    Some(r) => r.clone(),
                    None => continue,
                };
                if !poll_should_fetch(&poll_sid) {
                    continue;
                }
                let fetched = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    session::fetch_messages(&rt2, &poll_sid),
                )
                .await;
                match fetched {
                    Ok(Ok(msgs)) => {
                        fail_streak = 0;
                        backoff_fails = 0; // sucesso → backoff volta à cadência normal
                        // Itens completos (role, texto, kind) — reasoning vira
                        // mensagem DIM com badge THINK no transcript.
                        let list = session::extract_display_items(&msgs);
                        if list != snapshot {
                            let result = diff_lists(&snapshot, &list);
                            snapshot = list.clone();
                            if ui_tx2
                                .send(UiUpdate::Messages {
                                    sid: poll_sid.clone(),
                                    msgs: list,
                                    result,
                                })
                                .is_err()
                            {
                                break; // UI saiu — encerra junto com o loop
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        // Antes engolido (`.ok().and_then(|r| r.ok())`): falha
                        // de PROCESSO MORTO/IO LOCAL 3× seguidas = runtime
                        // morto → notice ÚNICA (flag) e segue em cadência
                        // idle. Erro de protocolo/RPC do servidor (Node VIVO)
                        // NÃO acumula o streak.
                        fail_streak = poll_fail_streak(fail_streak, &e);
                        // Toda falha REAL alonga o backoff (RPC/protocolo
                        // incluso — servidor vivo em apuros também é pollado
                        // mais devagar; o streak de morte segue como estava).
                        backoff_fails = backoff_fails.saturating_add(1);
                        if fail_streak >= POLL_DEAD_STREAK && !dead_notified {
                            dead_notified = true;
                            let notice = match runtime_exited_info(&e) {
                                Some((code, tail)) => {
                                    render::runtime_dead_banner(code, &tail)
                                }
                                None => format!(
                                    "⚠ runtime não responde ({e}). Se persistir, use /exit e reabra."
                                ),
                            };
                            if ui_tx2.send(UiUpdate::RuntimeDead { notice }).is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => {
                        // Envelope de 10s estourou: rede lenta — NÃO acumula
                        // (zera, igual ao timeout do runtime). Também NÃO mexe
                        // no backoff: não é falha de fato (só o sucesso reseta).
                        fail_streak = 0;
                    }
                }
            }
        });
    }

    // Ponte de notificações do runtime (E): qualquer push do filho desperta o
    // poller (fetch imediato) e marca a UI dirty. Nasce DENTRO da task de
    // boot (Fase V4-1), junto do runtime — antes ela era spawnada aqui, quando
    // o runtime ainda nascia antes da alternate screen. O shape dos eventos
    // NÃO é confirmado no protocolo — nada é interpretado além do nome do
    // método; sem notificações, o poll por intervalo segue (fallback).

    // Turno em background: usage antes/depois e send_and_wait ficam DENTRO da
    // task (rede fora da UI). O fim do turno é detectado por `is_finished`
    // (O(1)) e o resultado aplicado na task da UI.
    let mut send_task: Option<TurnJoin> = None;
    // Diff do ÚLTIMO turno concluído (p/ o /diff; escopo do loop por ser o
    // estado mais simples — não precisa sobreviver fora da task da UI).
    let mut last_ws_diff: Option<WorkspaceDiff> = None;
    let mut anim = tokio::time::interval(std::time::Duration::from_millis(tui::ANIM_TICK_MS));
    let mut dirty = true;

    loop {
        // Fim de turno: sem rede aqui (usage/diff/git já foram buscados na task).
        if send_task.as_ref().is_some_and(|h| h.is_finished()) {
            if let Some(h) = send_task.take() {
                // Desliga `working` ANTES do match: o JoinError (panic dentro
                // da task do turno) também precisa destravar a UI — antes o
                // `if let Ok` engolia e o spinner ficava eterno (A-3 da V3).
                app.working = false;
                // Sem `poll_wake.notify_one()` aqui de propósito: o fim de
                // turno não precisa de fetch imediato — a cadência idle
                // (1,5s) é aceitável e evita um poll extra por turno.
                let _ = working_tx.send(false);
                match h.await {
                    Ok(out) => {
                        last_ws_diff = out.ws_diff;
                        match (out.res, out.after) {
                            (Ok((_, _, proj)), Ok(u)) => {
                                let st = session::measure_turn(&out.before, &u, out.elapsed);
                                app.status = format!(
                                    "turno concluído em {:.1}s · {:.1} tok/s",
                                    st.elapsed_ms as f64 / 1000.0,
                                    st.tok_per_s
                                );
                                app.set_usage(u.input_tokens, u.output_tokens);
                                // Usage completo p/ os overlays (/context legenda,
                                // /usage cartão Session): reasoning/cache inclusos.
                                app.last_usage = Some(u);
                                app.last_stats = Some(st);
                                // Ctx% ao vivo: projection do `session/send`
                                // (parse_context já degrada p/ zeros — silencioso).
                                let c = session::parse_context(&proj);
                                if c.window > 0 {
                                    app.ctx = c;
                                }
                                // /todos (Fase V4-2): refresh defensivo da
                                // projection do send; ausente/vazio mantém o
                                // checklist anterior (nada inventado).
                                let todos = session::extract_todos(&proj);
                                if !todos.is_empty() {
                                    app.todos = todos;
                                }
                                // Atividade de tools (DEFENSIVA): shape de
                                // activeToolCalls não verificado — só emite com o
                                // shape esperado; qualquer outro, nada.
                                if let Some(t) = session::summarize_active_tools(&proj) {
                                    app.push_msg("system", &t);
                                }
                            }
                            (Ok(_), Err(_)) => app.status = "turno concluído".to_string(),
                            (Err(e), _) => {
                                // Runtime morto (Exited) ganha banner próprio e
                                // trava novos turnos; outros erros mantêm o texto.
                                if let Some((code, tail)) = runtime_exited_info(&e) {
                                    app.runtime_dead = true;
                                    app.status = "runtime morto".to_string();
                                    app.push_msg("system", &render::runtime_dead_banner(code, &tail));
                                } else {
                                    app.status = "turno falhou/parado".to_string();
                                }
                            }
                        }
                        // Diff pós-turno: notice única quando houve mudança
                        // (snapshot criado/modificado ou git stat).
                        if let Some(notice) =
                            format_diff_notice(last_ws_diff.as_ref(), out.git_stat.as_deref())
                        {
                            app.push_msg("system", &notice);
                        }
                    }
                    Err(e) => {
                        // JoinError: panic dentro da task do turno (o abort do
                        // Esc não passa por aqui — ele tira o handle antes).
                        // O panic hook já registrou o detalhe no log; aqui
                        // só destravamos a UI com um aviso honesto.
                        app.status = "turno interrompido (erro interno)".to_string();
                        app.push_msg("system", &turn_join_error_notice());
                        tracing::error!(erro = %e, "task do turno terminou sem resultado (panic/cancel)");
                    }
                }
                // Fase V5-1 (fila): turno TERMINOU (usage/diff/todos já
                // aplicados acima) e nenhum outro processamento aconteceu
                // ainda — se houver mensagem enfileirada, INICIA o próximo
                // turno AGORA, pelo mesmo caminho do Enter (`spawn_turn`).
                // Encadeia naturalmente: fila com 3 itens → um por turno.
                // Gates: runtime morto NÃO consome (o item fica na fila p/
                // `/queue clear` ou reabertura); sem runtime no slot, idem.
                if !app.queue.is_empty() && !app.runtime_dead {
                    if let Some(rt2) = rt_slot.get() {
                        if let Some(texto) = app.next_queued() {
                            app.push_msg("system", "enviando mensagem da fila…");
                            spawn_turn(
                                &mut app,
                                rt2,
                                &ws,
                                &mut send_task,
                                &working_tx,
                                &poll_wake,
                                &texto,
                            );
                        }
                    }
                }
                dirty = true;
            }
        }
        // Draw só quando dirty (C): teclado/mouse, UiUpdate ou animação.
        if dirty {
            dirty = false;
            term.draw(|f| tui::render(f, &mut app, &pal))
                .map_err(|e| CmdError::Io(format!("draw: {e}")))?;
        }
        if app.quit {
            break;
        }
        tokio::select! {
            _ = anim.tick() => {
                // Animação (spinner/statusbar) separada da rede: só marca
                // dirty enquanto há turno ativo; ocioso não desenha.
                if app.working {
                    app.tick += 1;
                    dirty = true;
                }
            }
            ev = rx.recv() => {
                let Some(ev) = ev else { break }; // thread de teclado morreu
                let mut kctx = KeyCtx {
                    app: &mut app,
                    created: &mut created,
                    // None até o boot entregar o runtime (Fase V4-1) — os
                    // braços que precisam de RPC tratam o vazio com notice.
                    rt: rt_slot.get(),
                    boot: &boot,
                    ws: &ws,
                    send_task: &mut send_task,
                    working_tx: &working_tx,
                    ui_tx: &ui_tx,
                    poll_wake: &poll_wake,
                    last_diff: &mut last_ws_diff,
                };
                // Só desenha se houve ação: tecla sempre; mouse só em scroll
                // da roda (Moved/Drag não provocam redraw). O dreno em lote
                // continua: várias teclas/scrolls → UM draw.
                let mut handled = handle_key_event(&mut kctx, ev);
                while let Ok(ev2) = rx.try_recv() {
                    handled |= handle_key_event(&mut kctx, ev2);
                }
                if handled {
                    dirty = true;
                }
            }
            upd = ui_rx.recv() => {
                if let Some(u) = upd {
                    apply_ui_update(&mut app, &mut created, &sid_tx, u);
                }
                // Coalescing: drena o lote inteiro de updates antes do draw.
                apply_pending_updates(&mut ui_rx, &mut app, &mut created, &sid_tx);
                dirty = true;
            }
        }
    }
    // Fase V4-1: sinaliza saída à task de boot (um filho nascido depois daqui
    // é derrubado LÁ, pelo check pós-spawn) e encerra o runtime que existir.
    // Sem sessão criada (usuário saiu durante o "conectando…"), nenhum
    // session/close — só a árvore do processo.
    quit_flag.store(true, Ordering::SeqCst);
    if let Some(rt) = rt_slot.get() {
        let sid_final = if app.session_ready && !app.session_id.is_empty() {
            Some(app.session_id.as_str())
        } else {
            None
        };
        shutdown(rt, sid_final).await;
    }
    Ok(())
}

// ----- paste por timing (Windows nativo; no-op em Unix) -----

/// Janela (ms) que separa colagem sintetizada de tecla real: paste chega com
/// gaps <1ms; digitação humana ≥50ms; auto-repeat de tecla ~33ms — 20ms não
/// captura repeat nem digitação.
const PASTE_GAP_MS: u64 = 20;

/// Núcleo da decisão (puro, testável): `gap_ms` é o tempo desde a ÚLTIMA tecla
/// vista. Só converte `Enter` SIMPLES (SHIFT/CONTROL são atalho, não paste),
/// `kind == Press` e com gap abaixo da janela — senão é tecla humana/repeat.
fn should_convert_enter(gap_ms: u128, ev: &Event) -> bool {
    matches!(
        ev,
        Event::Key(k)
            if k.code == KeyCode::Enter
                && k.kind == KeyEventKind::Press
                && k.modifiers.is_empty()
                && gap_ms < PASTE_GAP_MS as u128
    )
}

/// Decisão da thread de teclado (pura, testável): dado o instante da última
/// tecla (None = 1º evento da thread — sem anterior, NUNCA converte) e o
/// evento cru, devolve `Some(Event::Paste("\n"))` quando o Enter é colado.
/// Mouse/Resize/Focus não são Key → seguem direto (e o chamador nem consulta
/// o relógio para eles).
fn enter_to_newline(last_key_at: Option<std::time::Instant>, ev: &Event) -> Option<Event> {
    if !matches!(ev, Event::Key(_)) {
        return None;
    }
    let gap_ms = last_key_at?.elapsed().as_millis();
    should_convert_enter(gap_ms, ev).then(|| Event::Paste("\n".into()))
}

/// Resultado do merge incremental do poll de mensagens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeResult {
    /// Servidor idêntico ao estado local — sem rebuild (não custo por tick).
    Unchanged,
    /// Mensagens novas só no fim — append (follow do render é preservado).
    Appended,
    /// Divergência no meio (compact, edição server-side) — estado substituído
    /// com o mínimo de perturbação; o render re-ajusta o offset pelo `follow`.
    /// `dropped_local`: mensagens locais não-system sem par no servidor (ex.:
    /// envio que falhou antes de chegar lá) descartadas pelo merge — o apply
    /// avisa o usuário quando > 0.
    Replaced { dropped_local: usize },
}

/// Atualizações assíncronas para a task da UI. A task da UI é a ÚNICA dona do
/// `TuiApp`: nada compartilha estado mutável com as tasks auxiliares — tudo
/// chega por essa fila e é aplicado em `apply_ui_update` (B).
#[derive(Debug)]
pub enum UiUpdate {
    /// Lista autoritativa do servidor (poller): `sid` protege contra corrida
    /// com Ctrl+N (update de sessão antiga é descartado); `result` é o diff
    /// do poller contra o snapshot anterior. Itens completos (role, texto,
    /// kind) — reasoning entra como mensagem própria.
    Messages {
        sid: String,
        msgs: Vec<session::MsgItem>,
        result: MergeResult,
    },
    /// `session/usage` resolvido (Ctrl+U, /usage). `notice`: também registra
    /// linha de sistema com os totais.
    Usage {
        usage: session::Usage,
        notice: bool,
    },
    /// Linha de sistema (resultados de slash commands, erros de RPC).
    Notice(String),
    /// Sessão criada (subscribe já feito na task): boot da 1ª sessão
    /// (Fase V4-1) E Ctrl+N usam este caminho. `model` é o efetivo da sessão
    /// nova (flag --model quando passada, senão `settings.model` do result).
    NewSession {
        created: Value,
        sid: String,
        model: String,
    },
    /// Notificação push do runtime (shape não confirmado — só o método é
    /// propagado; serve para marcar a UI dirty).
    SessionEvent { method: String },
    /// Runtime morto detectado (task do turno ou poller): banner único +
    /// `app.runtime_dead = true` (trava novos turnos; header vira DEAD).
    RuntimeDead { notice: String },
    /// Resultado do `session/list` do picker de sessões (`/resume` sem
    /// argumento, Fase V5-2). `Ok(vazio)` → estado "vazio" do picker;
    /// `Err` → picker fecha com notice (o usuário já tem o transcript).
    SessionsLoaded { res: Result<Value, String> },
    /// Sessão retomada via `session/resume` (picker ou `/resume <id>`,
    /// Fase V5-2). Mesma projeção do `NewSession` (reset de transcript,
    /// watch do sid, busca fechada) com status/notice de resume. `quente`
    /// vem do R1 condicional (transporte Daemon → sessão POSSIVELMENTE
    /// quente; Embedded → leitura garantida).
    ResumedSession {
        resumed: Value,
        sid: String,
        model: String,
        quente: bool,
    },
}

type UiTx = tokio::sync::mpsc::UnboundedSender<UiUpdate>;
type UiRx = tokio::sync::mpsc::UnboundedReceiver<UiUpdate>;

/// Payload devolvido pela task do turno — tudo coletado FORA da task da UI
/// (rede, snapshots e git stat fora do caminho do draw). `res` traz o result
/// bruto do `session/send` (projection p/ ctx% e activeToolCalls).
struct TurnOutcome {
    before: session::Usage,
    res: Result<(Value, String, Value), session::SessionError>,
    elapsed: std::time::Duration,
    after: Result<session::Usage, session::SessionError>,
    /// Diff pós-turno (snapshot before/after); None = nada mudou no disco.
    ws_diff: Option<WorkspaceDiff>,
    /// `git diff --stat` do workspace pós-turno (None fora de repo git).
    git_stat: Option<String>,
}

type TurnJoin = tokio::task::JoinHandle<TurnOutcome>;

/// Quedas CONSECUTIVAS de processo morto/IO local do poller antes de
/// declarar runtime morto.
pub const POLL_DEAD_STREAK: u32 = 3;

/// Contador puro do poller: acumula SÓ erro de processo morto/IO local
/// (`RuntimeError::Exited` = filho sumiu; `RuntimeError::Io` = pipe local
/// quebrou). Timeout zera (rede lenta ≠ runtime morto) e erro de
/// protocolo/RPC do servidor TAMBÉM não acumula (Node vivo respondendo
/// erro ≠ queda): só contam quedas consecutivas — qualquer outro resultado
/// zera. Sucesso zera direto no braço de sucesso do loop.
pub fn poll_fail_streak(prev: u32, e: &session::SessionError) -> u32 {
    if poll_err_is_timeout(e) || !poll_err_is_local(e) {
        0
    } else {
        prev.saturating_add(1)
    }
}

/// O erro do fetch é timeout (rede lenta) e não queda do processo? (puro)
fn poll_err_is_timeout(e: &session::SessionError) -> bool {
    matches!(
        e,
        session::SessionError::Runtime(crate::runtime::RuntimeError::Timeout(..))
    )
}

/// O erro indica processo morto/IO LOCAL (queda real)? (puro) `Exited` =
/// filho sumiu (`try_wait` no `call`); `Io` do runtime = pipe local quebrou
/// (`write_all` falhou). `Rpc`/`Protocol`/`Config` = servidor VIVO
/// respondendo com erro — não é queda de runtime.
fn poll_err_is_local(e: &session::SessionError) -> bool {
    matches!(
        e,
        session::SessionError::Runtime(
            crate::runtime::RuntimeError::Exited(..) | crate::runtime::RuntimeError::Io(..)
        )
    )
}

/// Runtime morto? `SessionError::Runtime(#[from] Exited(code, stderr_tail))`
/// (puro; antes essa informação se perdia como "turno falhou/parado").
fn runtime_exited_info(e: &session::SessionError) -> Option<(Option<i32>, String)> {
    match e {
        session::SessionError::Runtime(crate::runtime::RuntimeError::Exited(code, tail)) => {
            Some((*code, tail.clone()))
        }
        _ => None,
    }
}

/// Resumo "+X −Y" da última linha do `git diff --stat` (puro). None se a
/// linha de resumo não trouxer inserções/deleções.
pub fn git_stat_summary(stat: &str) -> Option<String> {
    let last = stat.lines().last().unwrap_or("");
    let mut ins: Option<u64> = None;
    let mut del: Option<u64> = None;
    for seg in last.split(',') {
        let seg = seg.trim();
        let num: String = seg.chars().take_while(|c| c.is_ascii_digit()).collect();
        if num.is_empty() {
            continue;
        }
        let n: u64 = num.parse().ok()?;
        if seg.contains("insertion") {
            ins = Some(n);
        } else if seg.contains("deletion") {
            del = Some(n);
        }
    }
    match (ins, del) {
        (None, None) => None,
        (i, d) => {
            let mut s = String::new();
            if let Some(i) = i {
                s.push_str(&format!("+{i}"));
            }
            if let Some(d) = d {
                if !s.is_empty() {
                    s.push(' ');
                }
                s.push_str(&format!("−{d}"));
            }
            Some(s)
        }
    }
}

fn trunc_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// Notice de diff pós-turno p/ a TUI e /diff (puro). Referência visual: os
/// formatos do headless (created/modified + `git diff --stat`).
/// "editou: a.rs, b.rs — git: +12 −3". Lista limitada a 5 nomes.
pub fn format_diff_notice(wd: Option<&WorkspaceDiff>, git_stat: Option<&str>) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(wd) = wd {
        let mut files: Vec<String> = wd
            .created
            .iter()
            .chain(wd.modified.iter())
            .cloned()
            .collect();
        if !files.is_empty() {
            files.sort();
            files.dedup();
            const CAP: usize = 5;
            let extra = files.len().saturating_sub(CAP);
            files.truncate(CAP);
            let mut list = files.join(", ");
            if extra > 0 {
                list.push_str(&format!(" (+{extra})"));
            }
            parts.push(format!("editou: {list}"));
        }
    }
    if let Some(g) = git_stat {
        if let Some(sum) = git_stat_summary(g) {
            parts.push(format!("git: {sum}"));
        } else {
            let first = g.lines().next().unwrap_or("").trim();
            if !first.is_empty() {
                parts.push(format!("git: {}", trunc_chars(first, 48)));
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" — "))
    }
}

/// Cadence de poll do transcript (A/F): curto enquanto `working` (config,
/// clamp 100ms), longo ocioso. Puro para teste.
pub const IDLE_POLL_MS: u64 = 1500;

// ----- /export: dump da conversa (Fase V4-2) -----

/// Caminho default do export (puro): `./zcode-export-{sid8}.md` no cwd
/// (`.json` com `--json`). Sid vazio/curto degrada p/ "sess" (o gate do
/// Enter já garante sessão real na prática — só sanitário).
pub fn export_default_path(sid: &str, json: bool) -> String {
    let sid8 = session::sid_short8(sid);
    let nome = if sid8.is_empty() { "sess" } else { &sid8 };
    format!("zcode-export-{nome}.{}", if json { "json" } else { "md" })
}

/// Mensagens do histórico da TUI p/ o export: tudo exceto o marcador de
/// splash (notices system entram como role system, reasoning como kind).
fn export_tuples(app: &TuiApp) -> Vec<(String, String, session::MsgKind)> {
    app.messages
        .iter()
        .filter(|m| m.text != art::SPLASH_MARKER)
        .map(|m| (m.role.clone(), m.text.clone(), m.kind))
        .collect()
}

/// Escreve o arquivo do export (bloqueante — na TUI roda em
/// `spawn_blocking`, fora do caminho do draw): serializa (md ou json),
/// grava no caminho dado (ou no default do cwd) e devolve o caminho
/// ABSOLUTO p/ a notice.
fn write_export(
    msgs: &[(String, String, session::MsgKind)],
    sid: &str,
    model: &str,
    path: Option<&str>,
    json: bool,
) -> Result<std::path::PathBuf, String> {
    let quando = now_rfc3339();
    let (conteudo, default) = if json {
        let v = session::build_export_json(msgs, sid, model, &quando);
        (
            serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?,
            export_default_path(sid, true),
        )
    } else {
        (
            session::build_export_md(msgs, sid, model, &quando),
            export_default_path(sid, false),
        )
    };
    let alvo = std::path::PathBuf::from(path.unwrap_or(&default));
    std::fs::write(&alvo, conteudo).map_err(|e| e.to_string())?;
    // Absoluto p/ a notice (canonicalize falha em rede/UNC → junta com o cwd
    // como fallback honesto).
    std::fs::canonicalize(&alvo)
        .or_else(|_| std::env::current_dir().map(|d| d.join(&alvo)))
        .map_err(|e| e.to_string())
}


pub fn poll_interval_ms(working: bool, configured_ms: u64) -> u64 {
    if working {
        configured_ms.max(100)
    } else {
        IDLE_POLL_MS
    }
}

/// Teto do backoff do poller: com o servidor em apuros, no máximo 15s entre
/// fetches (em vez de martelar na cadência idle de 1,5s).
pub const POLL_BACKOFF_CAP_MS: u64 = 15_000;

/// Ctrl+C "de saída" (qualquer case, com CONTROL)? Usado pelo gate de overlay:
/// com o modal aberto, TODO evento o fecha e é consumido — exceto Ctrl+C, que
/// fecha o modal e segue para o fluxo normal de saída (2× saem mesmo com
/// modal). Puro, testável.
pub fn is_ctrl_c_event(ev: &Event) -> bool {
    matches!(ev,
        Event::Key(k)
            if k.code == crossterm::event::KeyCode::Char('c')
                && k.modifiers.contains(crossterm::event::KeyModifiers::CONTROL))
}

/// Gate do poller (Fase V4-1, puro): só fetch com sid REAL. O watch `sid`
/// começa no placeholder "" (startup instantâneo) e só sai dele no 1º
/// `UiUpdate::NewSession` — até lá o poller dorme na cadência normal, sem
/// gastar rede nem acumular streak/backoff.
pub fn poll_should_fetch(sid: &str) -> bool {
    !sid.is_empty()
}

/// Backoff exponencial do poller em falhas CONSECUTIVAS de fetch (puro):
/// `base × 2^fails`, saturando no teto de 15s. `fails == 0` devolve a
/// cadência base intacta (sem backoff). O reset é responsabilidade do
/// chamador: `fails` volta a 0 no PRIMEIRO sucesso (timeout de envelope não
/// conta — não incrementa nem reseta, não é falha de fato).
pub fn poll_backoff_ms(base_ms: u64, fails: u32) -> u64 {
    if fails == 0 {
        return base_ms;
    }
    // 2^fails com saturação do shift: `fails ≥ 64` estoura o u64 — clamp em
    // 63 (2^63 × qualquer base ≥ 1 já passa do cap; o min() resolve o resto).
    let mult = 1u64 << fails.min(63);
    base_ms.saturating_mul(mult).min(POLL_BACKOFF_CAP_MS)
}

/// Diff puro entre snapshots consecutivos do poller (mesma semântica do
/// `merge_messages`): decide o tipo de atualização antes de despachar. Compara
/// o item COMPLETO (role, texto, kind) — reasoning novo/divergente atualiza.
/// A contagem de descartes (`dropped_local`) só é conhecida no merge, na task
/// da UI — aqui fica 0.
fn diff_lists(prev: &[session::MsgItem], next: &[session::MsgItem]) -> MergeResult {
    if prev == next {
        MergeResult::Unchanged
    } else if next.len() > prev.len()
        && prev.iter().zip(next.iter()).all(|(a, b)| a == b)
    {
        MergeResult::Appended
    } else {
        MergeResult::Replaced { dropped_local: 0 }
    }
}

/// Aplica um `UiUpdate` NA task da UI (dona do TuiApp). Puro o suficiente
/// para teste: sem runtime, sem rede.
fn apply_ui_update(
    app: &mut TuiApp,
    created: &mut Value,
    sid_tx: &tokio::sync::watch::Sender<String>,
    u: UiUpdate,
) {
    match u {
        UiUpdate::Messages { sid, msgs, result } => {
            if sid != app.session_id {
                // Corrida com Ctrl+N: lista da sessão antiga não entra.
                tracing::trace!(%sid, "update de sessão antiga descartado");
                return;
            }
            let applied = merge_messages(&mut app.messages, &msgs);
            if applied != MergeResult::Unchanged {
                app.bump_rev(); // invalida o cache do transcript
            }
            if let MergeResult::Replaced { dropped_local } = applied {
                if dropped_local > 0 {
                    // Aviso ÚNICO (só quando > 0): o merge descartou envio(s)
                    // local(is) que o servidor nunca confirmou.
                    app.push_msg(
                        "system",
                        &format!(
                            "⚠ {dropped_local} mensagem(ns) locais não confirmadas \
                             pelo servidor foram removidas do histórico"
                        ),
                    );
                }
            }
            tracing::trace!(?result, ?applied, "poll aplicado");
        }
        UiUpdate::Usage { usage, notice } => {
            app.set_usage(usage.input_tokens, usage.output_tokens);
            // Guarda o usage COMPLETO (reasoning/cache/reqs) p/ os overlays.
            app.last_usage = Some(usage.clone());
            if notice {
                app.push_msg(
                    "system",
                    &format!(
                        "total={} in={} out={} reqs={}",
                        usage.total_tokens, usage.input_tokens, usage.output_tokens, usage.model_request_count
                    ),
                );
            }
        }
        // B-3 da V4: qualquer resposta do boot (sucesso ou falha) destrava o
        // single-flight — falsos destravamentos (Notice de outra fonte no
        // meio do boot) apenas re-habilitam o retry, nunca travam o usuário.
        UiUpdate::Notice(s) => {
            app.booting = false;
            app.push_msg("system", &s);
        }
        UiUpdate::NewSession { created: c, sid, model } => {
            let notice = format!("sessão nova: {sid}");
            apply_session_switch(
                app,
                created,
                sid_tx,
                c,
                sid,
                model,
                "pronto".to_string(),
                notice,
            );
        }
        UiUpdate::SessionsLoaded { res } => {
            match res {
                Ok(v) => {
                    // Picker fechado enquanto a busca rodava → descarte
                    // silencioso (o usuário já desistiu).
                    app.fill_session_picker(session::parse_session_list(&v));
                }
                Err(e) => {
                    // Falha do fetch: picker fecha (não fica preso em
                    // "buscando sessões…") e a notice carrega o erro.
                    app.picker = None;
                    app.status = "pronto".to_string();
                    app.push_msg("system", &format!("sessions falhou: {e}"));
                }
            }
        }
        UiUpdate::ResumedSession { resumed, sid, model, quente } => {
            // Status honesto pelo R1 condicional: transporte Daemon → a
            // sessão PODE estar quente (envio possível; se fria, o -32031
            // aparece no turno como hoje); Embedded → leitura garantida.
            let id8 = session::sid_short8(&sid);
            let status = if quente {
                format!("sessão retomada: {id8} (quente — pode enviar)")
            } else {
                format!("sessão retomada: {id8} (leitura — R1)")
            };
            apply_session_switch(
                app,
                created,
                sid_tx,
                resumed,
                sid,
                model,
                status.clone(),
                status,
            );
        }
        UiUpdate::SessionEvent { method } => {
            // Sem shape confirmado: nada a aplicar; o loop marca dirty.
            tracing::debug!(method, "evento do runtime (dirty)");
        }
        UiUpdate::RuntimeDead { notice } => {
            // Banner único (quem emite já guarda flag p/ não repetir) + trava
            // de novos turnos (Enter bloqueado; header vira DEAD).
            app.runtime_dead = true;
            app.status = "runtime morto".to_string();
            // B-3 da V4: o boot falhou (braço RuntimeDead) — destrava o retry.
            app.booting = false;
            app.push_msg("system", &notice);
        }
    }
}

/// Troca de sessão ativa — caminho ÚNICO do `UiUpdate::NewSession` (boot e
/// Ctrl+N) e do `UiUpdate::ResumedSession` (picker e `/resume <id>`,
/// Fase V5-2). Reseta o transcript local, o scroll e os overlays de telemetria,
/// destrava os gates (session_ready idempotente, booting, runtime_dead) e
/// atualiza o watch do sid — o poller troca de sessão e faz o fetch inicial
/// da sessão retomada pelo braço `sid_rx.changed()` (refetch imediato, sem
/// notify extra). `status` e `notice` separam o que aparece na statusbar do
/// que entra no transcript ("sessão nova: …" vs "sessão retomada: …").
/// M-1: itens ENFILEIRADOS pertencem à sessão abandonada — são descartados
/// com UMA notice (e o overlay aberto fecha junto), nunca vazam para a nova.
fn apply_session_switch(
    app: &mut TuiApp,
    created: &mut Value,
    sid_tx: &tokio::sync::watch::Sender<String>,
    c: Value,
    sid: String,
    model: String,
    status: String,
    notice: String,
) {
    *created = c;
    app.session_id = sid.clone();
    app.messages.clear();
    app.bump_rev();
    // M-1: fila da sessão antiga morre aqui (contagem p/ a notice abaixo) e
    // o overlay de telemetria aberto fecha — nada da sessão anterior vaza.
    let fila_descartada = app.queue_clear();
    app.overlay = None;
    app.scroll = 0;
    app.follow = true;
    app.ctx = session::parse_context(created);
    app.last_stats = None;
    // Usage da sessão anterior não vale para a nova (overlays limpos).
    app.last_usage = None;
    // Checklist do create (Fase V4-2: result.todos/todoGroups). No resume o
    // result não traz todos — limpar é o honesto (nada inventado, nada velho).
    app.todos = session::extract_todos(created);
    // Modelo efetivo da sessão (boot: flag --model vence; Ctrl+N:
    // settings.model do result; resume: settings do result retomado).
    if !model.is_empty() {
        app.model = model;
    }
    // Sessão nova/retomada só é possível com runtime vivo: limpa a trava.
    app.runtime_dead = false;
    // Busca da sessão antiga aponta para um transcript que não existe
    // mais: fecha restaurando o buffer do input (se a linha estava
    // aberta) e descarta query/matches — nada de saltos fantasma.
    if app.search.is_some() {
        app.close_search_keep();
        app.search = None;
    }
    // Fase V4-1: destrava Enter/poller (idempotente no Ctrl+N/resume).
    app.mark_session_ready();
    // B-3 da V4: boot concluído — destrava o single-flight do retry.
    app.booting = false;
    app.status = status;
    app.push_msg("system", &notice);
    // M-1: notice ÚNICA de fila descartada (só quando havia itens) — entra
    // depois da notice da troca, no transcript já limpo da sessão nova.
    if fila_descartada > 0 {
        app.push_msg(
            "system",
            &format!(
                "fila descartada na troca de sessão ({fila_descartada} mensagem/ns)"
            ),
        );
    }
    // Poller troca de sessão + reseta snapshot. Sem notify_one aqui:
    // o braço `sid_rx.changed()` do select do poller já interrompe o
    // sleep → refetch imediato (watch guarda a mudança pendente). Quando o
    // sid retomado é IGUAL ao atual, o `poll_wake` do spawn cobre o fetch.
    let _ = sid_tx.send(sid);
}

/// B-3 da V4 (puro, testável): Ctrl+N/Enter devem ser travados pelo
/// single-flight do boot? Sessão pendente + boot em voo → trava.
fn boot_single_flight_trava(session_ready: bool, booting: bool) -> bool {
    !session_ready && booting
}

/// Coalescing (C): drena a fila SEM await e aplica tudo — vários updates
/// resultam em UM draw. Retorna true se algo foi aplicado (→ dirty).
fn apply_pending_updates(
    rx: &mut UiRx,
    app: &mut TuiApp,
    created: &mut Value,
    sid_tx: &tokio::sync::watch::Sender<String>,
) -> bool {
    let mut any = false;
    while let Ok(u) = rx.try_recv() {
        apply_ui_update(app, created, sid_tx, u);
        any = true;
    }
    any
}

/// Estado mutável que o handler de teclas precisa — tudo pertence à task da
/// UI; RPCs saem por `tokio::spawn` e devolvem pelo canal (B). `rt` é
/// `Option` (Fase V4-1): None até o boot da 1ª sessão entregar o runtime —
/// os braços com RPC tratam o vazio com notice (na prática inalcançável com
/// `session_ready`, mas o handler nunca deve esperar). `boot` serve o retry
/// do Ctrl+N quando a sessão ainda não nasceu.
struct KeyCtx<'a> {
    app: &'a mut TuiApp,
    created: &'a mut Value,
    rt: Option<&'a Arc<Transport>>,
    boot: &'a SessionBoot,
    ws: &'a str,
    send_task: &'a mut Option<TurnJoin>,
    working_tx: &'a tokio::sync::watch::Sender<bool>,
    ui_tx: &'a UiTx,
    /// Gatilho do fetch imediato do poller (Enter → working).
    poll_wake: &'a std::sync::Arc<tokio::sync::Notify>,
    /// Diff do último turno concluído (p/ o /diff; vive no escopo do loop).
    last_diff: &'a mut Option<WorkspaceDiff>,
}

/// Fetch de `session/usage` em background (mesmo caminho do Ctrl+U): o
/// resultado chega pelo canal como `UiUpdate::Usage` (a task da UI guarda o
/// completo em `app.last_usage` p/ os overlays). `notice` = também registra
/// linha de sistema com os totais.
fn spawn_usage_fetch(rt: &Arc<Transport>, sid: &str, tx: &UiTx, notice: bool) {
    let rt2 = rt.clone();
    let sid2 = sid.to_string();
    let tx2 = tx.clone();
    tokio::spawn(async move {
        let upd = match session::fetch_usage(&rt2, &sid2).await {
            Ok(u) => UiUpdate::Usage { usage: u, notice },
            Err(e) => UiUpdate::Notice(format!("usage falhou: {e}")),
        };
        let _ = tx2.send(upd);
    });
}

/// Limite do fetch do picker (`session/list`) — mesmo valor default do
/// subcomando `sessions` (suficiente p/ escolher; o servidor trunca).
const SESSION_PICKER_LIMIT: u32 = 50;

/// Fetch da lista de sessões p/ o picker (`/resume` sem argumento, Fase
/// V5-2): RPC em spawn (nunca no caminho da tecla), resultado pelo canal.
/// `Ok` alimenta o picker aberto; `Err` fecha o picker com notice.
fn spawn_sessions_list(rt: &Arc<Transport>, tx: &UiTx) {
    let rt2 = rt.clone();
    let tx2 = tx.clone();
    tokio::spawn(async move {
        let res = session::list_sessions(&rt2, SESSION_PICKER_LIMIT)
            .await
            .map_err(|e| e.to_string());
        let _ = tx2.send(UiUpdate::SessionsLoaded { res });
    });
}

/// Troca de sessão SEM sair da TUI (`switch_session`, Fase V5-2): executa o
/// MESMO fluxo do `zcode-cli resume <id>` — `session/resume` (o servidor
/// pode devolver um sid NOVO; o resultado é a fonte da verdade) + subscribe
/// do poller no sid retomado + `UiUpdate::ResumedSession` pelo canal (o
/// apply reusa a projeção do NewSession). R1 condicional: com transporte
/// Daemon a sessão pode estar quente (envio possível); Embedded → leitura
/// garantida — o gate do Enter continua o de sempre (session_ready) e um
/// -32031 num turno frio aparece honestamente como hoje. O `poll_wake`
/// garante fetch imediato mesmo quando o sid retomado é o atual (o watch
/// não muda → o braço `changed()` não acordaria).
fn spawn_switch_session(rt: &Arc<Transport>, id: &str, tx: &UiTx, poll_wake: &std::sync::Arc<tokio::sync::Notify>) {
    let rt2 = rt.clone();
    let id2 = id.to_string();
    let tx2 = tx.clone();
    let wake2 = poll_wake.clone();
    let quente = daemon_client::r1_resume_may_send(rt.kind());
    tokio::spawn(async move {
        match rt2
            .call("session/resume", session::resume_params(&id2), 60)
            .await
        {
            Ok(res) => {
                // O resume pode devolver um sessionId diferente do pedido:
                // vale o que o servidor disse (fallback = id pedido).
                let sid = session::extract_session_id(&res).unwrap_or_else(|| id2.clone());
                if let Err(e) = rt2
                    .call("session/subscribe", tui_subscribe_params(&sid), 15)
                    .await
                {
                    tracing::warn!(%e, "subscribe falhou (poll mantido)");
                }
                let model = session::effective_model(&res);
                let _ = tx2.send(UiUpdate::ResumedSession { resumed: res, sid, model, quente });
                wake2.notify_one();
            }
            Err(e) => {
                let _ = tx2.send(UiUpdate::Notice(format!("resume falhou: {e}")));
            }
        }
    });
}

/// Inicia um turno com o texto dado — o MESMO caminho do Enter: estado
/// otimista no mesmo frame (user msg + working) e task de fundo (snapshot,
/// usage antes/depois, send, diff e git stat fora do caminho do draw).
/// Usado pelo Enter (texto do buffer) e pelo consumo da fila no fim do
/// turno (Fase V5-1). A expansão `@arquivo` acontece AQUI, no início do
/// pipeline do turno: o transcript guarda o texto COMO DIGITADO (com `@`)
/// e o que vai ao servidor é o expandido; avisos viram notices system
/// curtas. Leitura síncrona de ≤4 arquivos ≤48KB — sem await (o handler
/// responde no mesmo frame).
fn spawn_turn(
    app: &mut TuiApp,
    rt: &Arc<Transport>,
    ws: &str,
    send_task: &mut Option<TurnJoin>,
    working_tx: &tokio::sync::watch::Sender<bool>,
    poll_wake: &std::sync::Arc<tokio::sync::Notify>,
    texto: &str,
) {
    let texto = texto.trim();
    let (expandido, avisos) = session::expand_at_files(texto, ws);
    // Avisos ANTES da user msg: a mensagem segue como a última do transcript.
    for a in &avisos {
        app.push_msg("system", a);
    }
    // A-1: o transcript guarda o ORIGINAL; quando a expansão `@arquivo`
    // altera o texto, o EXPANDIDO vai como FIO (`wire`) na própria mensagem —
    // o merge do poll compara o eco do servidor contra o fio (`msg_eq`) e o
    // original permanece (sem `Replaced` espúrio nem aviso de descarte).
    let fio = if expandido == texto {
        None
    } else {
        Some(expandido.clone())
    };
    app.push_msg_with_wire("user", texto, fio);
    // Histórico de prompts p/ recall ↑/↓ — registra o ORIGINAL (com `@`).
    app.remember_prompt(texto);
    app.working = true;
    app.status = "working…".to_string();
    let sid2 = app.session_id.clone();
    let ws2 = ws.to_string();
    let rt2 = rt.clone();
    *send_task = Some(tokio::spawn(async move {
        let start = std::time::Instant::now();
        // Snapshot ANTES do send (diff pós-turno); o custo fica na
        // task, fora do caminho do draw (snapshot limitado a 20k).
        let snap_before = snapshot_workspace(&ws2);
        let before = session::fetch_usage(&rt2, &sid2).await.unwrap_or_default();
        // Triple: (poll bruto, texto, projection do session/send).
        // `expandido` = texto do usuário com @arquivo resolvido.
        let res = session::send_and_wait_fast(&rt2, &sid2, &expandido, 300).await;
        let elapsed = start.elapsed();
        let after = session::fetch_usage(&rt2, &sid2).await;
        // Pós-turno (ainda na task): snapshot + git stat.
        let snap_after = snapshot_workspace(&ws2);
        let wd = diff_snapshots(&snap_before, &snap_after);
        let git = git_diff_stat(&ws2);
        let ws_diff = if wd.created.is_empty()
            && wd.modified.is_empty()
            && git.is_none()
        {
            None
        } else {
            Some(wd)
        };
        TurnOutcome {
            before,
            res,
            elapsed,
            after,
            ws_diff,
            git_stat: git,
        }
    }));
    let _ = working_tx.send(true); // poller acelera
    // Acorda o poller AGORA: sem isso, o 1º fetch do turno só
    // aconteceria no fim do intervalo atual (working ou idle).
    poll_wake.notify_one();
}

/// Handler de teclado/mouse: só mutação local otimista + spawn de RPC. Nenhuma
/// await de rede aqui — a tecla responde no mesmo frame. Retorna `true` se o
/// evento foi consumido com efeito visível (→ dirty); o mouse só "conta" em
/// scroll da roda (Moved/Drag/cliques não provocam redraw).
fn handle_key_event(ctx: &mut KeyCtx, ev: Event) -> bool {
    use crossterm::event::MouseEventKind;

    // Overlay aberto (/context, /usage) tem prioridade MÁXIMA: qualquer evento
    // fecha o modal e é consumido — o char NÃO entra no buffer, paste NÃO
    // insere atrás da janela, scroll NÃO rola por baixo, Esc NÃO cancela
    // turno, vale inclusive durante `working`. ÚNICA exceção: Ctrl+C fecha o
    // modal e segue para o fluxo normal de saída (2× saem mesmo com modal).
    if ctx.app.overlay.is_some() {
        ctx.app.overlay = None;
        if !is_ctrl_c_event(&ev) {
            return true;
        }
    }
    // Picker de sessões aberto (/resume sem argumento, Fase V5-2): modal
    // INTERATIVO — ↑/↓ navegam, PgUp/PgDn pagina, Enter retoma, Esc fecha;
    // TODAS as outras teclas são consumidas sem efeito (nada vaza para o
    // input nem dispara atalhos). ÚNICA exceção, igual aos overlays:
    // Ctrl+C fecha o picker e segue para o fluxo normal de saída (2× saem).
    if ctx.app.picker.is_some() {
        if is_ctrl_c_event(&ev) {
            ctx.app.picker = None; // cai para o fluxo Ctrl+C logo abaixo
        } else if let Event::Key(kev) = ev {
            match tui::map_key(kev) {
                TuiKey::Up => ctx.app.picker.as_mut().unwrap().move_selected(-1),
                TuiKey::Down => ctx.app.picker.as_mut().unwrap().move_selected(1),
                TuiKey::PageUp => ctx
                    .app
                    .picker
                    .as_mut()
                    .unwrap()
                    .move_selected(-tui::PICKER_PAGE_STEP),
                TuiKey::PageDown => ctx
                    .app
                    .picker
                    .as_mut()
                    .unwrap()
                    .move_selected(tui::PICKER_PAGE_STEP),
                TuiKey::Enter => {
                    // Id escolhido → fecha o picker e dispara o MESMO fluxo
                    // do `zcode-cli resume <id>` (spawn; a troca efetiva
                    // acontece no `UiUpdate::ResumedSession`). Feedback via
                    // notice (padrão do /fork) — o status fica por conta do
                    // resultado, para não ficar preso se o RPC falhar.
                    // Loading/Empty → take devolve None e nada acontece.
                    if let Some(id) = ctx.app.take_selected_session() {
                        ctx.app.push_msg(
                            "system",
                            &format!(
                                "(retomando sessão {}…)",
                                session::sid_short8(&id)
                            ),
                        );
                        match ctx.rt {
                            Some(rt2) => {
                                spawn_switch_session(rt2, &id, ctx.ui_tx, ctx.poll_wake)
                            }
                            None => {
                                ctx.app.push_msg(
                                    "system",
                                    &render::session_pending_notice(),
                                );
                            }
                        }
                    }
                }
                TuiKey::Esc => ctx.app.close_session_picker(),
                _ => {} // demais teclas: consumidas sem efeito (modal)
            }
            return true;
        } else {
            // Mouse/Paste/Resize com picker aberto: consumidos (modal) —
            // Resize devolve true → redraw normal do frame.
            return true;
        }
    }
    // Busca aberta (Ctrl+F): a linha "find:" captura teclado e paste — chars
    // editam a query, Enter confirma/salta (vira sticky), Esc cancela. Só
    // Ctrl+C escapa (fecha a busca restaurando o buffer e segue o fluxo de
    // saída, como os overlays); scroll do mouse continua rolando o
    // transcript por baixo (não é texto da query).
    if ctx.app.search.as_ref().is_some_and(|s| s.open) {
        if let Event::Key(_) | Event::Paste(_) = &ev {
            if !is_ctrl_c_event(&ev) {
                return handle_search_event(ctx, ev);
            }
            ctx.app.cancel_search();
        }
    }
    if let Event::Mouse(m) = ev {
        // Roda do mouse no transcript; demais eventos do mouse ignorados.
        return match m.kind {
            MouseEventKind::ScrollUp => {
                ctx.app.scroll_up(3);
                true
            }
            MouseEventKind::ScrollDown => {
                ctx.app.scroll_down(3);
                true
            }
            _ => false,
        };
    }
    // Resize/Focus caem aqui: como antes, provocam redraw.
    // Paste (bracketed): o texto entra INTEIRO no buffer — quebras de linha
    // incluídas, saneadas (`\r\n`/`\r` → `\n`) — e NADA é enviado; o usuário
    // revisa e dá Enter. Sem suporte do terminal (ele nunca emite
    // `Event::Paste`), cada char do paste chega como `Event::Key` e um paste
    // multilinha envia linha a linha no 1º Enter — limitação do terminal
    // (crossterm 0.28 nem parseia paste no backend Windows; sem debouncing).
    if let Event::Paste(text) = ev {
        ctx.app.disarm_ctrlc();
        ctx.app.insert_str(&text);
        return true;
    }
    let Event::Key(kev) = ev else { return true };
    // y/n pendente (compact ou perm stub) consome a tecla.
    if (ctx.app.pending_compact || ctx.app.perm_stub.is_some())
        && matches!(tui::map_key(kev), TuiKey::Char(_) | TuiKey::Enter)
    {
        let yes = match tui::map_key(kev) {
            TuiKey::Char(c) => confirm_yes(&c.to_string()),
            _ => false,
        };
        match ctx.app.confirm_pending(yes) {
            Some(d) if d == "compact:yes" => {
                // RPC fora do caminho crítico (pode levar minutos): otimista
                // "(compactando…)" + resultado pelo canal. A confirmação y/n
                // continua aqui (fluxo local preservado).
                ctx.app
                    .push_msg("system", "(compactando… a sessão pode levar um minuto.)");
                if let Some(rt2) = ctx.rt.cloned() {
                    let sid2 = ctx.app.session_id.clone();
                    let tx2 = ctx.ui_tx.clone();
                    tokio::spawn(async move {
                        let msg = match session::compact_session(&rt2, &sid2).await {
                            Ok(_) => "(sessão compactada.)".to_string(),
                            Err(e) => format!("compact falhou: {e}"),
                        };
                        let _ = tx2.send(UiUpdate::Notice(msg));
                    });
                }
            }
            Some(d) => ctx.app.push_msg("system", &d),
            None => {}
        }
        ctx.app.input.clear();
        ctx.app.cursor = 0;
        return true;
    }
    match tui::map_key(kev) {
        TuiKey::Ignore => {}
        TuiKey::Char(c) => {
            ctx.app.disarm_ctrlc();
            ctx.app.insert_char(c);
        }
        TuiKey::Newline => {
            ctx.app.disarm_ctrlc();
            ctx.app.newline();
        }
        TuiKey::Backspace => ctx.app.backspace(),
        TuiKey::Delete => ctx.app.delete(),
        TuiKey::Left => ctx.app.move_left(),
        TuiKey::Right => ctx.app.move_right(),
        // ↑/↓ com recall condicional do histórico (buffer vazio/single-line);
        // buffer multilinha mantém a navegação de cursor. Ctrl+↑/Ctrl+↓
        // navegam SEM restrição de linha.
        TuiKey::Up => ctx.app.up_key(),
        TuiKey::Down => ctx.app.down_key(),
        TuiKey::CtrlUp => ctx.app.history_up_force(),
        TuiKey::CtrlDown => ctx.app.history_down_force(),
        // Home/End (e Ctrl+Home/Ctrl+End) rolam o transcript — decisão
        // explícita; o input não usa atalhos de cursor.
        TuiKey::Home | TuiKey::CtrlHome => ctx.app.scroll_home(),
        TuiKey::End | TuiKey::CtrlEnd => ctx.app.scroll_end(),
        // Página real: a altura interna medida no último render (o render
        // gruda/clampa o offset no fundo de qualquer forma).
        TuiKey::PageUp => ctx.app.scroll_up(ctx.app.page_step()),
        TuiKey::PageDown => ctx.app.scroll_down(ctx.app.page_step()),
        TuiKey::Esc => {
            if ctx.send_task.is_some() {
                if let Some(h) = ctx.send_task.take() {
                    h.abort();
                }
                // Otimista: solta a UI já; o RPC de stop vai em background.
                ctx.app.working = false;
                let _ = ctx.working_tx.send(false);
                ctx.app.push_msg("system", "(turn parado — prompt limpo.)");
                if let Some(rt2) = ctx.rt.cloned() {
                    let sid2 = ctx.app.session_id.clone();
                    let tx2 = ctx.ui_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = session::stop_turn(&rt2, &sid2).await {
                            let _ = tx2.send(UiUpdate::Notice(format!("stop falhou: {e}")));
                        }
                    });
                }
            }
        }
        TuiKey::CtrlC => {
            // 2º Ctrl+C marca quit; o loop sai após o draw.
            ctx.app.ctrlc();
        }
        TuiKey::CtrlR => {
            // R1: retomada = leitura; sem RPC novo (zero gasto).
            ctx.app.push_msg("system", &render::resume_readonly_notice());
        }
        TuiKey::CtrlN => {
            // B-3 da V4 (single-flight do boot): boot já em voo (key-repeat/
            // duplo Ctrl+N com a sessão pendente) → notice curta e NADA.
            if boot_single_flight_trava(ctx.app.session_ready, ctx.app.booting) {
                ctx.app.push_msg("system", "conectando (aguarde)…");
                return true;
            }
            ctx.app.push_msg("system", "(criando sessão…)");
            if !ctx.app.session_ready {
                // Fase V4-1 (startup instantâneo): a 1ª sessão ainda não
                // nasceu (boot em andamento ou falhou) — retry COMPLETO pela
                // mesma task de boot (respawna o runtime se preciso). B-3:
                // `booting` trava novos disparos até a resposta chegar.
                ctx.app.booting = true;
                spawn_session_boot(ctx.boot, ctx.ui_tx, ctx.ws);
            } else if let Some(rt2) = ctx.rt.cloned() {
                let ws2 = ctx.ws.to_string();
                let tx2 = ctx.ui_tx.clone();
                tokio::spawn(async move {
                    match session::create_session(&rt2, &ws2).await {
                        Ok(c) => {
                            let sid = session::extract_session_id(&c).unwrap_or_default();
                            if let Err(e) = rt2
                                .call("session/subscribe", tui_subscribe_params(&sid), 15)
                                .await
                            {
                                tracing::warn!(%e, "subscribe falhou (poll mantido)");
                            }
                            // Modelo efetivo da sessão nova (settings.model).
                            let model = session::effective_model(&c);
                            let _ = tx2.send(UiUpdate::NewSession { created: c, sid, model });
                        }
                        Err(e) => {
                            let _ = tx2.send(UiUpdate::Notice(format!("new falhou: {e}")));
                        }
                    }
                });
            }
        }
        TuiKey::CtrlU => {
            match ctx.rt {
                Some(rt) => {
                    spawn_usage_fetch(rt, &ctx.app.session_id, ctx.ui_tx, false);
                }
                None => {
                    // Fase V4-1: sem runtime ainda (boot em andamento).
                    ctx.app.push_msg("system", &render::session_pending_notice());
                }
            }
        }
        TuiKey::CtrlP => {
            // Tempo 2: sem RPC de approve/deny — componente oculto + nota.
            ctx.app.push_msg("system", &render::perm_unsupported_note());
        }
        TuiKey::CtrlF => {
            // Busca no transcript (Fase V4-2): linha "find:" no lugar do
            // input — buffer guardado, restaurado ao fechar. Reabre a sticky.
            ctx.app.disarm_ctrlc();
            ctx.app.open_search();
        }
        // F3/Shift+F3 com a linha fechada = navegação da busca sticky
        // (Enter a deixou); sem busca/matches, no-op silencioso.
        TuiKey::F3 => {
            ctx.app.disarm_ctrlc();
            ctx.app.search_step(1);
        }
        TuiKey::ShiftF3 => {
            ctx.app.disarm_ctrlc();
            ctx.app.search_step(-1);
        }
        TuiKey::Enter => {
            ctx.app.disarm_ctrlc();
            let buf = std::mem::take(&mut ctx.app.input);
            ctx.app.cursor = 0;
            if buf.trim().is_empty() {
                return true;
            }
            // Fase V4-1 (startup instantâneo): sessão ainda não existe — Enter
            // bloqueado com notice curta, SEM spawn de turno/RPC e SEM perder
            // o buffer (volta ao input, como o read_only faz). B-3: durante o
            // boot em voo a notice é honesta sobre a espera.
            if !ctx.app.session_ready {
                if ctx.app.booting {
                    ctx.app.push_msg("system", "conectando (aguarde)…");
                } else {
                    ctx.app.push_msg("system", &render::session_pending_notice());
                }
                ctx.app.input = buf;
                ctx.app.cursor = ctx.app.input_len();
                return true;
            }
            // Slash de uma linha → comandos Fase 3; resto → turno.
            let slash = !buf.contains('\n') && buf.trim_start().starts_with('/');
            if slash {
                match ctx.rt {
                    Some(rt) => handle_tui_slash(
                        rt,
                        ctx.app,
                        ctx.created,
                        buf.trim(),
                        ctx.ui_tx,
                        ctx.ws,
                        ctx.last_diff,
                        ctx.poll_wake,
                    ),
                    None => {
                        // Inalcançável na prática (slash já passou o gate
                        // session_ready ⇒ boot completo) — defensivo.
                        ctx.app.push_msg("system", &render::session_pending_notice());
                    }
                }
            } else if ctx.app.read_only {
                ctx.app.push_msg("system", &render::resume_readonly_notice());
                ctx.app.input = buf;
                ctx.app.cursor = ctx.app.input_len();
            } else if ctx.app.runtime_dead {
                // Runtime morto: SEM spawn de turno (o processo Node não existe
                // mais); sugere o caminho de recuperação e devolve o texto.
                ctx.app.push_msg("system", &render::runtime_dead_blocked());
                ctx.app.input = buf;
                ctx.app.cursor = ctx.app.input_len();
            } else if ctx.app.working {
                // Fase V5-1 (fila estilo Codex/Claude): em vez de devolver o
                // texto com "aguarde", ENFILEIRA — o buffer fica limpo (o
                // texto foi "enviado para a fila"). O item é enviado
                // automaticamente quando o turno termina (consumo no fim do
                // h.await, pelo `spawn_turn`). A-2: o histórico de prompts
                // registra SÓ no consumo real (spawn_turn) — enfileirar não
                // é enviar; B-1: sem status dedicado — o badge `[fila: N]`
                // do prompt/spinner já informa a contagem.
                ctx.app.queue_push(buf.trim());
            } else {
                // Defensivo (Fase V4-1): runtime ausente sem session_ready é
                // impossível (o gate acima já devolveu o buffer) — nunca
                // dispara working sem task p/ encerrá-lo.
                let Some(rt2) = ctx.rt.cloned() else {
                    ctx.app.push_msg("system", &render::session_pending_notice());
                    ctx.app.input = buf;
                    ctx.app.cursor = ctx.app.input_len();
                    return true;
                };
                spawn_turn(
                    ctx.app,
                    &rt2,
                    ctx.ws,
                    ctx.send_task,
                    ctx.working_tx,
                    ctx.poll_wake,
                    &buf,
                );
            }
        }
    }
    true // tecla consumida (inclusive Ignore) — mantém o redraw imediato
}

/// Teclado da linha de busca (Ctrl+F aberta): chars editam a query (com
/// cursor, mesma unidade do editor do input), ↑/↓ e F3/Shift+F3 navegam os
/// matches SALTANDO junto, Enter confirma (fecha + salta; busca fica sticky
/// com F3/Shift+F3 de fora) e Esc cancela (buffer restaurado, busca
/// descartada). Demais teclas são consumidas sem efeito — nada vaza para o
/// input nem para os atalhos do loop.
fn handle_search_event(ctx: &mut KeyCtx, ev: Event) -> bool {
    if let Event::Paste(text) = ev {
        ctx.app.search_insert_str(&text);
        return true;
    }
    let Event::Key(kev) = ev else { return false };
    match tui::map_key(kev) {
        TuiKey::Char(c) => ctx.app.search_insert_char(c),
        TuiKey::Backspace => ctx.app.search_backspace(),
        TuiKey::Delete => ctx.app.search_delete(),
        TuiKey::Left => ctx.app.search_move_left(),
        TuiKey::Right => ctx.app.search_move_right(),
        TuiKey::Home => ctx.app.search_move_home(),
        TuiKey::End => ctx.app.search_move_end(),
        // ↑ = match anterior (como histórico), ↓/F3 = próximo, Shift+F3 =
        // anterior — todos saltam no ato.
        TuiKey::Up | TuiKey::ShiftF3 => ctx.app.search_step(-1),
        TuiKey::Down | TuiKey::F3 => ctx.app.search_step(1),
        TuiKey::Enter => ctx.app.search_confirm_jump(),
        TuiKey::Esc => ctx.app.cancel_search(),
        _ => return false, // PageUp/Ctrl+J/etc: consumidos sem efeito na busca
    }
    true
}

/// Mensagem local bate com o item do servidor (role, texto E kind — reasoning
/// novo/divergente tem que atualizar, não ser ignorado)?
/// A-1: com FIO na mensagem local (envio com `@arquivo`), o texto comparado é
/// o EXPANDIDO guardado em `wire` — o eco do servidor bate com o fio e a
/// local ORIGINAL segue no transcript. Sem fio, comparação direta (de sempre).
fn msg_eq(m: &ChatMsg, s: &session::MsgItem) -> bool {
    m.role == s.role
        && m.kind == s.kind
        && match &m.wire {
            Some(fio) => fio == &s.text,
            None => m.text == s.text,
        }
}

/// Merge incremental do poll (puro, testável): o servidor é a fonte da
/// verdade para user/assistant; mensagens locais `system` (splash, notices)
/// são preservadas no lugar.
/// - idêntico → `Unchanged` (nada é tocado — scroll nem rebuild);
/// - novos só no fim → `Appended` (append preserva o follow);
/// - divergiu no meio → `Replaced`: o i-ésimo não-system vira o i-ésimo do
///   servidor, notices system ficam no lugar, excedente do servidor é anexado
///   e não-system local sem correspondente é descartado (contado em
///   `dropped_local` p/ o apply avisar o usuário).
/// A-1 (fio): a comparação via `msg_eq` usa o texto EXPANDIDO guardado em
/// `wire` quando a local user o tem — o eco do servidor do `@arquivo` não
/// diverge (sem `Replaced` espúrio nem aviso de descarte) e a local
/// ORIGINAL permanece exibida. `Replaced` só acontece em divergência REAL
/// (servidor ≠ fio ≠ exibido); a mensagem substituta vem sem fio (o texto
/// dela JÁ É do servidor, estável no próximo poll).
fn merge_messages(messages: &mut Vec<ChatMsg>, server: &[session::MsgItem]) -> MergeResult {
    let local: Vec<&ChatMsg> = messages.iter().filter(|m| m.role != "system").collect();
    if local.len() == server.len() && local.iter().zip(server).all(|(m, s)| msg_eq(m, s)) {
        return MergeResult::Unchanged;
    }
    let n = local.len().min(server.len());
    if server.len() > local.len()
        && local[..n].iter().zip(&server[..n]).all(|(m, s)| msg_eq(m, s))
    {
        for s in &server[local.len()..] {
            messages.push(ChatMsg {
                role: s.role.clone(),
                text: s.text.clone(),
                kind: s.kind,
                wire: None,
            });
        }
        return MergeResult::Appended;
    }
    let mut dropped_local = 0usize;
    let mut out: Vec<ChatMsg> = Vec::with_capacity(messages.len() - local.len() + server.len());
    let mut it = server.iter();
    for m in messages.drain(..) {
        if m.role == "system" {
            out.push(m);
        } else if let Some(s) = it.next() {
            out.push(ChatMsg {
                role: s.role.clone(),
                text: s.text.clone(),
                kind: s.kind,
                wire: None,
            });
        } else {
            // Local sem par no servidor (ex.: envio que falhou) — descartado,
            // mas contado p/ o apply injetar o aviso único.
            dropped_local += 1;
        }
    }
    out.extend(it.map(|s| ChatMsg {
        role: s.role.clone(),
        text: s.text.clone(),
        kind: s.kind,
        wire: None,
    }));
    *messages = out;
    MergeResult::Replaced { dropped_local }
}

/// Slash commands Fase 3 dentro da TUI (mensagens viram linhas de sistema).
/// SEM RPC no caminho da tecla: validações locais rodam aqui e TODO RPC vai
/// para `tokio::spawn`, devolvendo o resultado pelo canal (`UiUpdate`) — a
/// task da UI aplica. Estados otimistas (mode/model/thought) são aplicados
/// já na tecla; falhas chegam como `Notice` (B).
fn handle_tui_slash(
    rt: &Arc<Transport>,
    app: &mut TuiApp,
    created: &Value,
    line: &str,
    tx: &UiTx,
    ws: &str,
    last_diff: &Option<WorkspaceDiff>,
    poll_wake: &std::sync::Arc<tokio::sync::Notify>,
) {
    use crate::ui::input;
    match input::parse_input(line) {
        input::Input::Empty | input::Input::Exit => app.quit = true,
        input::Input::Unknown(c) => app.push_msg(
            "system",
            &format!("comando desconhecido '{c}'. Use /help p/ a lista de comandos e atalhos."),
        ),
        input::Input::Help => {
            // 100% local (sem RPC): comandos + atalhos como notice system.
            app.push_msg("system", &render::help_text());
        }
        input::Input::Diff => {
            // git stat fresco (processo local, fora da tecla via
            // spawn_blocking) + diff do snapshot do último turno.
            let tx2 = tx.clone();
            let ws2 = ws.to_string();
            let last = last_diff.clone();
            tokio::spawn(async move {
                let stat = tokio::task::spawn_blocking(move || git_diff_stat(&ws2))
                    .await
                    .ok()
                    .flatten();
                let msg = format_diff_notice(last.as_ref(), stat.as_deref()).unwrap_or_else(
                    || "(sem mudanças: nem snapshot de turno nem diff git no workspace)".into(),
                );
                let _ = tx2.send(UiUpdate::Notice(msg));
            });
        }
        input::Input::Usage => {
            // /usage agora abre o OVERLAY (referência Claude Code): dois
            // cartões (Session / Last turn). Sem usage guardado ainda, dispara
            // o fetch do Ctrl+U em background p/ popular (`UiUpdate::Usage`
            // guarda o completo em `app.last_usage`).
            app.overlay = Some(tui::Overlay::Usage);
            app.status = "usage — qualquer tecla fecha".to_string();
            if app.last_usage.is_none() {
                spawn_usage_fetch(rt, &app.session_id, tx, false);
            }
        }
        input::Input::Context => {
            // /context: overlay com barra + legenda por tipo de token. A
            // legenda vem de `last_usage` — mesmo fetch em background quando
            // ainda não há nenhum (ctx% ao vivo já existe desde o create).
            app.overlay = Some(tui::Overlay::Context);
            app.status = "contexto — qualquer tecla fecha".to_string();
            if app.last_usage.is_none() {
                spawn_usage_fetch(rt, &app.session_id, tx, false);
            }
        }
        input::Input::Todos => {
            // /todos (Fase V4-2): overlay com o checklist do agente — dados
            // 100% locais (create + fim de cada turno), sem RPC na tecla.
            app.overlay = Some(tui::Overlay::Todos);
            app.status = "todos — qualquer tecla fecha".to_string();
        }
        input::Input::Queue(clear) => {
            // /queue (Fase V5-1): inspeção/limpeza deliberada da fila —
            // 100% local, sem RPC. Clear é a ÚNICA via de descarte (Esc
            // cancela o turno, não mexe na fila).
            if clear {
                let n = app.queue_clear();
                app.push_msg(
                    "system",
                    &format!("fila limpa ({n} mensagem(ns) descartada(s))."),
                );
            } else if app.queue.is_empty() {
                app.push_msg("system", "fila vazia.");
            } else {
                // Itens truncados (1ª linha) só p/ leitura — a fila continua
                // guardando o texto integral.
                let itens: Vec<String> = app
                    .queue
                    .iter()
                    .map(|t| trunc_chars(t.lines().next().unwrap_or(""), 40))
                    .collect();
                app.push_msg(
                    "system",
                    &format!("fila ({}): {}", app.queue.len(), itens.join(" | ")),
                );
            }
        }
        input::Input::Export { path, json } => {
            // /export (Fase V4-2): dump local da conversa — sem RPC. A
            // escrita (arquivo no cwd) vai para spawn_blocking: a tecla não
            // espera I/O; o resultado volta como Notice (caminho absoluto).
            let msgs = export_tuples(app);
            let sid = app.session_id.clone();
            let model = app.model.clone();
            app.push_msg("system", "(exportando conversa…)");
            let tx2 = tx.clone();
            tokio::spawn(async move {
                let res = tokio::task::spawn_blocking(move || {
                    write_export(&msgs, &sid, &model, path.as_deref(), json)
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r);
                let msg = match res {
                    Ok(p) => format!("exportado: {}", p.display()),
                    Err(e) => format!("export falhou: {e}"),
                };
                let _ = tx2.send(UiUpdate::Notice(msg));
            });
        }
        input::Input::Stop => {
            let rt2 = rt.clone();
            let sid = app.session_id.clone();
            let tx2 = tx.clone();
            tokio::spawn(async move {
                let msg = match session::stop_turn(&rt2, &sid).await {
                    Ok(_) => "(turn parado — prompt limpo.)".to_string(),
                    Err(e) => format!("stop falhou: {e}"),
                };
                let _ = tx2.send(UiUpdate::Notice(msg));
            });
        }
        input::Input::Mode(None) => app.push_msg("system", "uso: /mode <plan|build|edit|yolo>"),
        input::Input::Mode(Some(m)) => {
            app.mode = m.clone(); // otimista; falha chega pelo canal
            app.push_msg("system", &format!("modo: {m}"));
            let rt2 = rt.clone();
            let sid = app.session_id.clone();
            let tx2 = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = session::apply_mode(&rt2, &sid, &m).await {
                    let _ = tx2.send(UiUpdate::Notice(format!("mode falhou: {e}")));
                }
            });
        }
        input::Input::Model(None) => {
            let ms = session::available_models(created);
            app.push_msg("system", &render::format_models_list(&ms));
        }
        input::Input::Model(Some(r)) => {
            app.model = r.clone(); // otimista
            app.push_msg("system", &format!("modelo: {r}"));
            let rt2 = rt.clone();
            let sid = app.session_id.clone();
            let tx2 = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = session::apply_model(&rt2, &sid, &r).await {
                    let _ = tx2.send(UiUpdate::Notice(format!(
                        "model falhou: {e} (R3: ao vivo só zai/glm-5.3-Flash existe)"
                    )));
                }
            });
        }
        input::Input::Thought(None) => app.push_msg("system", "uso: /thought <low|high|max>"),
        input::Input::Thought(Some(l)) => {
            app.push_msg("system", &format!("thought-level: {l}"));
            let rt2 = rt.clone();
            let sid = app.session_id.clone();
            let tx2 = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = session::apply_thought(&rt2, &sid, &l).await {
                    let _ = tx2.send(UiUpdate::Notice(format!("thought falhou: {e}")));
                }
            });
        }
        input::Input::Compact => {
            // /compact também pede confirmação na TUI (gasta plano); o RPC só
            // dispara depois do y (handler de tecla, também em background).
            app.pending_compact = true;
            app.push_msg("system", &render::compact_confirm_prompt());
        }
        input::Input::Resume(id) => {
            // Fase V5-2 (picker de sessões): /resume SEM argumento abre o
            // picker (lista de session/list); /resume <id> dispara o MESMO
            // fluxo de retomada (o mesmo do Enter no picker). Durante um
            // turno ativo a troca é bloqueada (o turno pertence à sessão
            // atual; Esc cancela antes).
            if app.working {
                app.push_msg(
                    "system",
                    "(turno em andamento — Esc cancela antes de trocar de sessão.)",
                );
                return;
            }
            match id {
                None => {
                    app.open_session_picker();
                    spawn_sessions_list(rt, tx);
                }
                Some(id) => {
                    if let Err(e) = validate_resume_id(&id) {
                        app.push_msg("system", &format!("resume falhou: {e}"));
                        return;
                    }
                    app.push_msg(
                        "system",
                        &format!(
                            "(retomando sessão {}…)",
                            session::sid_short8(&id)
                        ),
                    );
                    spawn_switch_session(rt, &id, tx, poll_wake);
                }
            }
        }
        input::Input::New(None) => app.push_msg("system", "uso: Ctrl+N cria sessão nova aqui; ou saia e rode `new <pasta>`."),
        input::Input::New(Some(p)) => {
            let ws = config::normalize_workspace(&p);
            match validate_workspace_exists(&ws) {
                Ok(_) => app.push_msg("system", &format!("sessão nova: Ctrl+N aqui, ou saia e rode `new {ws}`.")),
                Err(e) => app.push_msg("system", &format!("new falhou: {e}")),
            }
        }
        input::Input::Fork(None) => app.push_msg("system", "uso: /fork <id>"),
        input::Input::Fork(Some(id)) => {
            if let Err(e) = validate_resume_id(&id) {
                app.push_msg("system", &format!("fork falhou: {e}"));
                return;
            }
            app.push_msg("system", &format!("(fork {id} em andamento…)"));
            let rt2 = rt.clone();
            let tx2 = tx.clone();
            tokio::spawn(async move {
                let msg = match rt2.call("session/fork", session::fork_params(&id), 60).await {
                    Ok(res) => {
                        let new_id = session::extract_session_id(&res).unwrap_or_default();
                        format!("fork: {id} → {new_id}")
                    }
                    Err(e) => {
                        format!("fork falhou: {e}. {}", render::fork_no_checkpoint_hint())
                    }
                };
                let _ = tx2.send(UiUpdate::Notice(msg));
            });
        }
        input::Input::Goal(action) => {
            let a = action.unwrap_or_else(|| "show".to_string());
            if goal_write_blocked(&a) {
                app.push_msg("system", &render::goal_write_blocked_hint());
                return;
            }
            let rt2 = rt.clone();
            let sid = app.session_id.clone();
            let tx2 = tx.clone();
            tokio::spawn(async move {
                let msg = match session::goal_action(&rt2, &sid, &a).await {
                    Ok(res) => serde_json::to_string_pretty(&res).unwrap_or_else(|_| "{}".into()),
                    Err(e) => format!("goal falhou: {e}"),
                };
                let _ = tx2.send(UiUpdate::Notice(msg));
            });
        }
        input::Input::Text(_) => app.push_msg("system", &render::resume_readonly_notice()),
    }
}

/// Mostra o histórico da sessão retomada (leitura) + aviso R1.
async fn show_resumed_reading(rt: &Arc<Transport>, session_id: &str) {
    match session::fetch_messages(rt, session_id).await {
        Ok(msgs) => {
            let list = session::extract_text_messages(&msgs);
            if list.is_empty() {
                println!("(sessão {session_id} sem mensagens)");
            } else {
                for (role, text) in list.iter().take(20) {
                    println!("{}", render::format_message(role, text));
                }
            }
        }
        Err(e) => eprintln!("leitura da sessão falhou: {e}"),
    }
    println!("{}", render::resume_readonly_notice());
}

async fn repl_loop(
    rt: &Arc<Transport>,
    session_id: &str,
    cli: &Cli,
    created: &Value,
    retro: bool,
    ws: &str,
) -> Result<(), CmdError> {
    println!("{}", art::splash_stdout(term_width()));
    if retro {
        println!("{}", render::welcome_retro(session_id));
    } else {
        println!("{}", render::welcome(session_id));
    }
    // Última lista numerada mostrada por `/resume` (Fase V5-2): `resume N`
    // escolhe o id da N-ésima linha. Vazia até o 1º `/resume` sem id.
    let mut picker_ids: Vec<String> = Vec::new();
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        print!("{}", theme::prompt());
        std::io::stdout()
            .flush()
            .map_err(|e| CmdError::Io(e.to_string()))?;
        let line = match lines.next() {
            Some(Ok(l)) => l,
            _ => break,
        };
        match input::parse_input(&line) {
            input::Input::Empty => continue,
            input::Input::Exit => break,
            input::Input::Unknown(c) => {
                eprintln!("comando desconhecido '{c}'. Use /help p/ a lista de comandos.");
                continue;
            }
            input::Input::Help => {
                println!("{}", render::help_text());
                continue;
            }
            input::Input::Diff => {
                // REPL não guarda snapshot por turno: só o git stat local.
                match git_diff_stat(ws) {
                    Some(s) => println!("{s}"),
                    None => println!("(sem diff git no workspace {ws})"),
                }
                continue;
            }
            input::Input::Context => {
                // Resumo textual barato (sem RPC): projection do create.
                println!("{}", tui::context_summary_line(&session::parse_context(created)));
                continue;
            }
            input::Input::Todos => {
                // Checklist do create (o REPL não guarda a projection dos
                // turnos — send_and_wait descarta por design; sem RPC novo).
                println!("{}", render::format_todos(&session::extract_todos(created)));
                continue;
            }
            input::Input::Queue(_) => {
                // Fila é recurso da TUI: o REPL é bloqueante (um turno por
                // vez), não há o que enfileirar. Notice honesta.
                println!("fila: indisponível no REPL (um turno por vez).");
                continue;
            }
            input::Input::Export { path, json } => {
                // /export no REPL: busca o transcript do servidor (leitura,
                // mesma RPC do boot) e escreve o arquivo local.
                match session::fetch_messages(rt, session_id).await {
                    Ok(msgs) => {
                        let tuples: Vec<(String, String, session::MsgKind)> =
                            session::extract_display_items(&msgs)
                                .into_iter()
                                .map(|i| (i.role, i.text, i.kind))
                                .collect();
                        let model = session::effective_model(created);
                        match write_export(&tuples, session_id, &model, path.as_deref(), json) {
                            Ok(p) => println!("exportado: {}", p.display()),
                            Err(e) => eprintln!("export falhou: {e}"),
                        }
                    }
                    Err(e) => eprintln!("export falhou: {e}"),
                }
                continue;
            }
            input::Input::Usage => {
                match session::fetch_usage(rt, session_id).await {
                    Ok(u) => println!(
                        "total={} in={} out={} reqs={}",
                        u.total_tokens, u.input_tokens, u.output_tokens, u.model_request_count
                    ),
                    Err(e) => eprintln!("usage falhou: {e}"),
                }
                continue;
            }
            input::Input::Stop => {
                match session::stop_turn(rt, session_id).await {
                    Ok(_) => println!("(turn parado — prompt limpo.)"),
                    Err(e) => eprintln!("stop falhou: {e}"),
                }
                continue;
            }
            input::Input::Mode(None) => {
                eprintln!("uso: /mode <plan|build|edit|yolo>");
                continue;
            }
            input::Input::Mode(Some(m)) => {
                match session::apply_mode(rt, session_id, &m).await {
                    Ok(_) => println!("modo: {m}"),
                    Err(e) => eprintln!("mode falhou: {e}"),
                }
                continue;
            }
            input::Input::Model(None) => {
                let ms = session::available_models(created);
                println!("{}", render::format_models_list(&ms));
                continue;
            }
            input::Input::Model(Some(r)) => {
                match session::apply_model(rt, session_id, &r).await {
                    Ok(_) => println!("modelo: {r}"),
                    Err(e) => {
                        eprintln!("model falhou: {e} (R3: ao vivo só zai/glm-5.3-Flash existe)")
                    }
                }
                continue;
            }
            input::Input::Thought(None) => {
                eprintln!("uso: /thought <low|high|max>");
                continue;
            }
            input::Input::Thought(Some(l)) => {
                match session::apply_thought(rt, session_id, &l).await {
                    Ok(_) => println!("thought-level: {l}"),
                    Err(e) => eprintln!("thought falhou: {e}"),
                }
                continue;
            }
            input::Input::Compact => {
                println!("{}", render::compact_confirm_prompt());
                print!("{}", theme::prompt());
                std::io::stdout()
                    .flush()
                    .map_err(|e| CmdError::Io(e.to_string()))?;
                let ans = lines
                    .next()
                    .unwrap_or(Ok(String::new()))
                    .unwrap_or_default();
                if !confirm_yes(&ans) {
                    println!("(compact cancelado.)");
                    continue;
                }
                match session::compact_session(rt, session_id).await {
                    Ok(_) => println!("(sessão compactada.)"),
                    Err(e) => eprintln!("compact falhou: {e}"),
                }
                continue;
            }
            input::Input::Resume(arg) => {
                // Fase V5-2: `/resume` sem id → lista numerada de
                // `session/list` (RPC de LEITURA — mesmo do subcomando
                // `sessions`); `resume N` escolhe da última lista. Nesta
                // REPL QUENTE não chamamos `session/resume` de propósito:
                // retomar outra sessão desativaria a NOSSA no runtime e
                // quebraria os envios seguintes — o honesto é apontar o
                // caminho (sair e `zcode-cli resume <id>`).
                match arg {
                    None => match session::list_sessions(rt, 50).await {
                        Ok(v) => {
                            let rows = session::parse_session_list(&v);
                            println!(
                                "{}",
                                render::format_session_list_numbered(&rows)
                            );
                            picker_ids = rows.into_iter().map(|r| r.id).collect();
                        }
                        Err(e) => eprintln!("sessions falhou: {e}"),
                    },
                    Some(a) => {
                        let escolhido = a
                            .parse::<usize>()
                            .ok()
                            .and_then(|n| n.checked_sub(1))
                            .and_then(|i| picker_ids.get(i).cloned());
                        match escolhido {
                            Some(id) => {
                                println!("resume {a} → {id}");
                                println!("{}", render::resume_readonly_notice());
                                println!(
                                    "para continuar NESSA sessão: saia (/exit) e rode `zcode-cli resume {id}`."
                                );
                            }
                            None => {
                                if a.parse::<usize>().is_ok() {
                                    eprintln!(
                                        "resume: {a} fora da lista (use /resume sem id p/ listar)."
                                    );
                                } else {
                                    // Id explícito: comportamento de sempre
                                    // (R1 — sem RPC aqui, mesma razão).
                                    println!("{}", render::resume_readonly_notice());
                                }
                            }
                        }
                    }
                }
                continue;
            }
            input::Input::New(None) => {
                eprintln!("uso: /new <pasta> (ou saia e rode: new <pasta>)");
                continue;
            }
            input::Input::New(Some(p)) => {
                let ws = config::normalize_workspace(&p);
                if let Err(e) = validate_workspace_exists(&ws) {
                    eprintln!("new falhou: {e}");
                    continue;
                }
                println!("sessão nova: saia (/exit) e rode `new {ws}` — ou `zcode-cli --cwd {ws}` sem args.");
                continue;
            }
            input::Input::Fork(None) => {
                eprintln!("uso: /fork <id>");
                continue;
            }
            input::Input::Fork(Some(id)) => {
                if let Err(e) = validate_resume_id(&id) {
                    eprintln!("fork falhou: {e}");
                    continue;
                }
                match rt.call("session/fork", session::fork_params(&id), 60).await {
                    Ok(res) => {
                        let new_id = session::extract_session_id(&res).unwrap_or_default();
                        println!("fork: {id} → {new_id}");
                    }
                    Err(e) => eprintln!("fork falhou: {e}. {}", render::fork_no_checkpoint_hint()),
                }
                continue;
            }
            input::Input::Goal(action) => {
                let a = action.unwrap_or_else(|| "show".to_string());
                if goal_write_blocked(&a) {
                    eprintln!("{}", render::goal_write_blocked_hint());
                    continue;
                }
                match session::goal_action(rt, session_id, &a).await {
                    Ok(res) => println!(
                        "{}",
                        serde_json::to_string_pretty(&res).unwrap_or_else(|_| "{}".into())
                    ),
                    Err(e) => eprintln!("goal falhou: {e}"),
                }
                continue;
            }
            input::Input::Text(t) => {
                // @arquivo (Fase V5-1): expansão no início do pipeline do
                // turno — o servidor recebe o texto expandido; avisos (token
                // não resolvido, limite de 4) vão ao stderr.
                let (expandido, avisos) = session::expand_at_files(&t, ws);
                for a in &avisos {
                    eprintln!("{a}");
                }
                match session::send_and_wait(rt, session_id, &expandido, 300).await {
                    Ok((_raw, text)) => {
                        if cli.json {
                            println!(
                                "{}",
                                serde_json::json!({ "sessionId": session_id, "response": text })
                            );
                        } else {
                            println!("{text}");
                        }
                    }
                    Err(e) => eprintln!("erro: {e}"),
                }
            }
        }
    }
    Ok(())
}

/// REPL de leitura (R1): não envia turnos; só /usage, /goal show e /exit
/// executam — o resto recebe o aviso R1 (zero gasto).
async fn repl_loop_readonly(
    rt: &Arc<Transport>,
    session_id: &str,
    cli: &Cli,
    retro: bool,
) -> Result<(), CmdError> {
    println!("{}", art::splash_stdout(term_width()));
    if retro {
        println!("{}", render::welcome_retro(session_id));
    } else {
        println!("{}", render::welcome(session_id));
    }
    println!("{}", render::resume_readonly_notice());
    // Alvo de leitura OWNED (Fase V5-2): `/resume` pode TROCÁ-LO — este REPL
    // é 100% leitura (sem envio), então retomar outra sessão é seguro aqui
    // (o runtime re-aponta; o poll... aqui não há poll: as leituras seguintes
    // passam a mirar o novo id).
    let mut alvo = session_id.to_string();
    // Última lista numerada mostrada por `/resume` (mesmo contrato da REPL
    // quente): `resume N` escolhe o id da N-ésima linha.
    let mut picker_ids: Vec<String> = Vec::new();
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        print!("{}", theme::prompt());
        std::io::stdout()
            .flush()
            .map_err(|e| CmdError::Io(e.to_string()))?;
        let line = match lines.next() {
            Some(Ok(l)) => l,
            _ => break,
        };
        match input::parse_input(&line) {
            input::Input::Empty => continue,
            input::Input::Exit => break,
            input::Input::Unknown(c) => {
                eprintln!("comando desconhecido '{c}'. Leitura: /help /usage /goal show /exit.");
                continue;
            }
            input::Input::Help => {
                println!("{}", render::help_text());
                continue;
            }
            input::Input::Usage => match session::fetch_usage(rt, &alvo).await {
                Ok(u) => println!(
                    "total={} in={} out={} reqs={}",
                    u.total_tokens, u.input_tokens, u.output_tokens, u.model_request_count
                ),
                Err(e) => eprintln!("usage falhou: {e}"),
            },
            input::Input::Goal(action) => {
                let a = action.unwrap_or_else(|| "show".to_string());
                if a.trim() != "show" {
                    println!("{}", render::resume_readonly_notice());
                    continue;
                }
                match session::goal_action(rt, &alvo, &a).await {
                    Ok(res) => println!(
                        "{}",
                        serde_json::to_string_pretty(&res).unwrap_or_else(|_| "{}".into())
                    ),
                    Err(e) => eprintln!("goal falhou: {e}"),
                }
            }
            input::Input::Export { path, json } => {
                // /export é leitura pura (session/messages + arquivo local):
                // permitido também no REPL readonly (mesma política do /usage).
                match session::fetch_messages(rt, &alvo).await {
                    Ok(msgs) => {
                        let tuples: Vec<(String, String, session::MsgKind)> =
                            session::extract_display_items(&msgs)
                                .into_iter()
                                .map(|i| (i.role, i.text, i.kind))
                                .collect();
                        match write_export(&tuples, &alvo, "", path.as_deref(), json) {
                            Ok(p) => println!("exportado: {}", p.display()),
                            Err(e) => eprintln!("export falhou: {e}"),
                        }
                    }
                    Err(e) => eprintln!("export falhou: {e}"),
                }
            }
            input::Input::Text(_) => {
                if cli.json {
                    println!(
                        "{}",
                        serde_json::json!({ "sessionId": alvo, "readOnly": true })
                    );
                } else {
                    println!("{}", render::resume_readonly_notice());
                }
            }
            input::Input::Resume(arg) => {
                // Fase V5-2: neste REPL (100% LEITURA) retomar outra sessão
                // é seguro — `session/resume` re-aponta o runtime e as
                // leituras seguintes miram o novo id (que pode ser DIFERENTE
                // do pedido: vale o que o servidor devolver, fallback = id).
                // `/resume` sem id → lista numerada; `resume N` escolhe dela.
                let alvo_novo = match arg {
                    None => match session::list_sessions(rt, 50).await {
                        Ok(v) => {
                            let rows = session::parse_session_list(&v);
                            println!(
                                "{}",
                                render::format_session_list_numbered(&rows)
                            );
                            picker_ids = rows.into_iter().map(|r| r.id).collect();
                            None
                        }
                        Err(e) => {
                            eprintln!("sessions falhou: {e}");
                            None
                        }
                    },
                    Some(a) => {
                        let escolhido = a
                            .parse::<usize>()
                            .ok()
                            .and_then(|n| n.checked_sub(1))
                            .and_then(|i| picker_ids.get(i).cloned());
                        match escolhido {
                            Some(id) => Some(id),
                            None if a.parse::<usize>().is_ok() => {
                                eprintln!(
                                    "resume: {a} fora da lista (use /resume sem id p/ listar)."
                                );
                                None
                            }
                            None => match validate_resume_id(&a) {
                                Ok(()) => Some(a),
                                Err(e) => {
                                    eprintln!("resume falhou: {e}");
                                    None
                                }
                            },
                        }
                    }
                };
                if let Some(id) = alvo_novo {
                    match rt
                        .call("session/resume", session::resume_params(&id), 60)
                        .await
                    {
                        Ok(res) => {
                            alvo = session::extract_session_id(&res)
                                .unwrap_or_else(|| id.clone());
                            println!("sessão retomada: {alvo}");
                            show_resumed_reading(rt, &alvo).await;
                        }
                        Err(e) => eprintln!("resume falhou: {e}"),
                    }
                }
            }
            _ => {
                println!("{}", render::resume_readonly_notice());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{input as ui_input, render as ui_render};

    #[test]
    fn ux_input_vazio_volta_ao_prompt() {
        assert_eq!(ui_input::parse_input(""), ui_input::Input::Empty);
        assert_eq!(ui_input::parse_input("   "), ui_input::Input::Empty);
    }

    #[test]
    fn ux_workspace_inexistente_erro_claro() {
        let e = validate_workspace_exists("C:/caminho/que/nao/existe-xyz-123").unwrap_err();
        assert!(e.to_string().contains("workspace inexistente"));
    }

    #[test]
    fn ux_resume_id_invalido_erro_claro() {
        assert!(validate_resume_id("").is_err());
        assert!(validate_resume_id("   ").is_err());
        assert!(validate_resume_id("sess_abc").is_ok());
        let e = validate_resume_id("").unwrap_err();
        assert!(e.to_string().contains("resume id inválido"));
    }

    #[test]
    fn ux_json_valido_one_shot_shape() {
        // Shape que o REPL/--json emite (não quebra headless do back).
        let v = serde_json::json!({"sessionId": "sess_1", "response": "ok"});
        let s = serde_json::to_string(&v).unwrap();
        assert!(serde_json::from_str::<Value>(&s).is_ok());
        assert_eq!(v["sessionId"], "sess_1");
    }

    #[test]
    fn ux_sessions_tabela_tem_colunas() {
        let v = serde_json::json!({"sessions":[{"sessionId":"sess_1","title":"t","mode":"build","status":"active","createdAt":"2026-09-08"}]});
        let t = ui_render::format_sessions_table(&v);
        for col in ["id", "titulo", "modo", "status", "data"] {
            assert!(t.contains(col), "falta coluna {col}");
        }
    }

    #[test]
    fn fase3_goal_write_bloqueado_com_hint() {
        // set/replace: sem RPC (campo de texto não verificado) — fixture pura.
        assert!(goal_write_blocked("set"));
        assert!(goal_write_blocked("replace"));
        assert!(!goal_write_blocked("show"));
        assert!(!goal_write_blocked("pause"));
        assert!(ui_render::goal_write_blocked_hint().contains("show|pause|resume|clear"));
    }

    #[test]
    fn fase3_compact_exige_confirmacao() {
        assert!(confirm_yes("y"));
        assert!(confirm_yes("yes"));
        assert!(confirm_yes(" S "));
        assert!(!confirm_yes("n"));
        assert!(!confirm_yes(""));
        assert!(ui_render::compact_confirm_prompt().contains("GASTA PLANO"));
    }

    #[test]
    fn fase3_goal_invalido_erro_claro() {
        let e = session::validate_goal_action("delete").unwrap_err();
        assert!(e.to_string().contains("goal action inválida"));
    }

    #[test]
    fn fase3_modelos_fixture_sem_chamada() {
        // available_models vem do create (sem nova chamada) — fixture.
        let create: Value = serde_json::from_str(
            r#"{"settings":{"model":{
              "current":{"providerId":"zai","modelId":"glm-5.3-Flash"},
              "available":[
                {"label":"glm-5.3-Flash","ref":{"providerId":"zai","modelId":"glm-5.3-Flash"},"contextWindow":1000000}
              ]}}}"#,
        )
        .unwrap();
        let ms = session::available_models(&create);
        assert_eq!(ms.len(), 1);
        let t = ui_render::format_models_list(&ms);
        assert!(t.contains("zai/glm-5.3-Flash"));
        assert!(session::validate_mode("turbo").is_err());
        assert!(session::validate_thought("medio").is_err());
    }

    #[test]
    fn fase3_fork_sem_id_erro_claro() {
        let e = validate_resume_id("   ").unwrap_err();
        assert!(e.to_string().contains("resume id inválido"));
        assert!(ui_render::fork_no_checkpoint_hint().contains("R4"));
    }

    #[test]
    fn fase4_subscribe_delivery_fixture() {
        // Contrato-fase4-5 §1: deliveryKind obrigatório (sem ele → -32602).
        let p = tui_subscribe_params("sess_x");
        assert_eq!(p["sessionId"], "sess_x");
        assert_eq!(p["deliveryKind"], "desktop-continuous");
    }

    // ----- panic hook: relatório no log + linha curta no stderr (puras) -----

    #[test]
    fn panic_report_e_stderr_line_shape() {
        // Relatório p/ o ARQUIVO: local + payload + newline final (append
        // convive com as linhas do tracing no mesmo log.txt).
        let r = panic_report("índice fora dos limites", "src/ui/tui.rs:1234:9");
        assert!(r.contains("panic em src/ui/tui.rs:1234:9"), "{r}");
        assert!(r.contains("payload: índice fora dos limites"), "{r}");
        assert!(r.ends_with('\n'), "termina em nova linha");
        // Linha curta do stderr: começa pelo local, cita o log e o doctor.
        let l = panic_stderr_line(
            "src/commands.rs:99:5",
            r"C:\Users\x\AppData\Roaming\zcode-cli\log.txt",
        );
        assert!(l.starts_with("panic em src/commands.rs:99:5"), "{l}");
        assert!(l.contains("zcode-cli\\log.txt"), "{l}");
        assert!(l.contains("zcode-cli doctor"), "{l}");
        assert!(
            l.lines().count() == 1,
            "uma linha só (terminal sujo = mensagem perdida)"
        );
        // Local desconhecido (panic sem location) não quebra o formato.
        assert!(panic_stderr_line("local desconhecido", "log.txt").contains("panic em"));
    }

    #[test]
    fn fase5_exit_codes() {
        // 0 sucesso (convenção); 1 erro; 2 turno parado.
        assert_eq!(exit_code(&CmdError::Runtime("x".into())), 1);
        assert_eq!(exit_code(&CmdError::Session("x".into())), 1);
        assert_eq!(exit_code(&CmdError::Config("x".into())), 1);
        assert_eq!(exit_code(&CmdError::Io("x".into())), 1);
        assert_eq!(exit_code(&CmdError::Stopped("timeout (300s)".into())), 2);
        // Timeout do send_and_wait vira Stopped.
        let e = CmdError::Session("timeout (300s) sem resposta do assistant".into());
        assert!(is_turn_stalled(&e));
        assert!(!is_turn_stalled(&CmdError::Session("outro erro".into())));
        assert!(!is_turn_stalled(&CmdError::Runtime("timeout".into())));
    }

    #[test]
    fn fase5_tools_filter_parse() {
        assert!(parse_tools_filter(None).is_empty());
        assert!(parse_tools_filter(Some("")).is_empty());
        assert_eq!(
            parse_tools_filter(Some("Bash(git *), Edit")),
            vec!["Bash(git *)", "Edit"]
        );
        assert_eq!(parse_tools_filter(Some("Bash Read")), vec!["Bash", "Read"]);
        assert_eq!(
            parse_tools_filter(Some("  Bash ,,  Read ")),
            vec!["Bash", "Read"]
        );
        // Sem filtros → sem aviso; com filtros → aviso honesto (sem setter RPC).
        assert!(tools_filter_warning(&[], &[]).is_none());
        let w = tools_filter_warning(&["Bash".into()], &[]).unwrap();
        assert!(w.contains("não aplicado ao protocolo"));
    }

    #[test]
    fn fase5_diff_snapshots_fixture() {
        use std::collections::HashMap;
        let before: HashMap<String, u64> = [("a.txt".into(), 10), ("b.txt".into(), 20)].into();
        let after: HashMap<String, u64> = [
            ("a.txt".into(), 10),
            ("b.txt".into(), 30),
            ("novo.txt".into(), 5),
        ]
        .into();
        let d = diff_snapshots(&before, &after);
        assert_eq!(d.created, vec!["novo.txt".to_string()]);
        assert_eq!(d.modified, vec!["b.txt".to_string()]);
        let empty = diff_snapshots(&before, &before);
        assert!(empty.created.is_empty() && empty.modified.is_empty());
    }

    #[test]
    fn fase5_snapshot_e_git_fixture() {
        // snapshot_workspace em dir temporária real (sem runtime).
        let dir = std::env::temp_dir().join(format!("zc-fase5-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "oi").unwrap();
        // Caches/VCS não entram no snapshot (perf + menos ruído no diff).
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("node_modules").join("x.js"), "junk").unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git").join("HEAD"), "ref").unwrap();
        std::fs::create_dir_all(dir.join(".venv")).unwrap();
        std::fs::write(dir.join(".venv").join("pyvenv.cfg"), "c").unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("main.rs"), "fn").unwrap();
        let snap = snapshot_workspace(&dir.to_string_lossy());
        assert!(snap.contains_key("f.txt"));
        assert!(snap.contains_key("src/main.rs"));
        assert!(!snap.keys().any(|k| k.contains("node_modules")));
        assert!(!snap.keys().any(|k| k.contains(".git")));
        assert!(!snap.keys().any(|k| k.contains(".venv")));
        // Não é repo git (ou git ausente) → None, sem erro.
        assert!(git_diff_stat(&dir.to_string_lossy()).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_skip_dir_cobre_caches_e_ocultos() {
        assert!(snapshot_skip_dir(".git"));
        assert!(snapshot_skip_dir("node_modules"));
        assert!(snapshot_skip_dir("target"));
        assert!(snapshot_skip_dir("__pycache__"));
        assert!(snapshot_skip_dir(".venv"));
        assert!(snapshot_skip_dir(".cache"));
        // Qualquer oculto (defesa: não vasar .env etc. no diff de paths).
        assert!(snapshot_skip_dir(".github"));
        assert!(!snapshot_skip_dir("src"));
        assert!(!snapshot_skip_dir("docs"));
        assert!(!snapshot_skip_dir("assets"));
    }

    #[test]
    fn fase5_notify_action() {
        assert_eq!(notify_action(false, None), NotifyAction::None);
        assert_eq!(notify_action(false, Some("x")), NotifyAction::None);
        assert_eq!(notify_action(true, None), NotifyAction::Beep);
        assert_eq!(notify_action(true, Some("  ")), NotifyAction::Beep);
        assert_eq!(
            notify_action(true, Some("cmd pronto")),
            NotifyAction::Command("cmd pronto".into())
        );
    }

    #[test]
    fn fase5_json_estendido_shape() {
        // Shape que o one_shot --json emite (contrato p/ orquestração).
        let v = serde_json::json!({
            "sessionId": "sess_1", "response": "ok",
            "usage": {"inputTokens": 1},
            "tools": {"allowed": ["Bash"], "disallowed": [], "warning": "w"},
            "diff": {"created": ["novo.txt"], "modified": [], "gitStat": "s"},
        });
        assert_eq!(v["diff"]["created"][0], "novo.txt");
        assert_eq!(v["tools"]["allowed"][0], "Bash");
        assert!(v["tools"]["warning"].is_string());
    }

    // ----- merge incremental do poll (scroll-friendly) -----

    fn cm(role: &str, text: &str) -> ChatMsg {
        ChatMsg {
            role: role.into(),
            text: text.into(),
            kind: crate::session::MsgKind::Text,
            wire: None,
        }
    }

    /// Item local COM fio (A-1): `text` exibido (original) + `wire` (o que
    /// foi efetivamente enviado — ex.: `@arquivo` já expandido).
    fn cmw(role: &str, text: &str, wire: &str) -> ChatMsg {
        ChatMsg {
            role: role.into(),
            text: text.into(),
            kind: crate::session::MsgKind::Text,
            wire: Some(wire.into()),
        }
    }

    /// Item do servidor (role, texto, kind=Text) p/ os fixtures do merge.
    fn mi(role: &str, text: &str) -> session::MsgItem {
        session::MsgItem {
            role: role.into(),
            text: text.into(),
            kind: crate::session::MsgKind::Text,
        }
    }

    #[test]
    fn merge_unchanged_nao_rebuild_por_tick() {
        let mut msgs = vec![cm("system", "splash"), cm("user", "oi"), cm("assistant", "olá")];
        let antes = msgs.clone();
        let server = vec![mi("user", "oi"), mi("assistant", "olá")];
        assert_eq!(merge_messages(&mut msgs, &server), MergeResult::Unchanged);
        assert_eq!(msgs, antes, "estado local intocado (nem rebuild nem scroll)");
    }

    #[test]
    fn merge_appended_preserva_system_e_follow() {
        let mut msgs = vec![cm("system", "splash"), cm("user", "oi")];
        let server = vec![mi("user", "oi"), mi("assistant", "olá")];
        assert_eq!(merge_messages(&mut msgs, &server), MergeResult::Appended);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0], cm("system", "splash"));
        assert_eq!(msgs[2], cm("assistant", "olá"));
    }

    #[test]
    fn merge_do_vazio_e_append() {
        let mut msgs: Vec<ChatMsg> = Vec::new();
        let server = vec![mi("user", "oi")];
        assert_eq!(merge_messages(&mut msgs, &server), MergeResult::Appended);
        assert_eq!(msgs, vec![cm("user", "oi")]);
        // Servidor vazio + local vazio = Unchanged.
        let mut vazio: Vec<ChatMsg> = Vec::new();
        assert_eq!(merge_messages(&mut vazio, &[]), MergeResult::Unchanged);
    }

    #[test]
    fn merge_reasoning_item_completo_diverge() {
        // Mesmo (role, texto) mas kind diferente (reasoning novo no servidor)
        // → NÃO é Unchanged: o merge substitui pelo item completo.
        let mut msgs = vec![cm("assistant", "mesma frase")];
        let server = vec![session::MsgItem {
            role: "assistant".into(),
            text: "mesma frase".into(),
            kind: crate::session::MsgKind::Reasoning,
        }];
        assert_eq!(
            merge_messages(&mut msgs, &server),
            MergeResult::Replaced { dropped_local: 0 }
        );
        assert_eq!(msgs[0].kind, crate::session::MsgKind::Reasoning);
        // E reasoning anexado no fim entra como Appended preservando system.
        let mut msgs2 = vec![cm("system", "splash"), cm("assistant", "olá")];
        let server2 = vec![
            mi("assistant", "olá"),
            session::MsgItem {
                role: "assistant".into(),
                text: "pensando".into(),
                kind: crate::session::MsgKind::Reasoning,
            },
        ];
        assert_eq!(merge_messages(&mut msgs2, &server2), MergeResult::Appended);
        assert_eq!(msgs2.len(), 3);
        assert_eq!(msgs2[2].kind, crate::session::MsgKind::Reasoning);
    }

    #[test]
    fn merge_replaced_divergencia_no_meio() {
        let mut msgs = vec![
            cm("system", "splash"),
            cm("user", "velha"),
            cm("assistant", "resposta velha"),
        ];
        let server = vec![mi("user", "velha"), mi("assistant", "resposta nova (pós-compact)")];
        assert_eq!(
            merge_messages(&mut msgs, &server),
            MergeResult::Replaced { dropped_local: 0 }
        );
        assert_eq!(msgs.len(), 3, "notice system preservado no lugar");
        assert_eq!(msgs[0], cm("system", "splash"));
        assert_eq!(msgs[1], cm("user", "velha"));
        assert_eq!(msgs[2], cm("assistant", "resposta nova (pós-compact)"));
    }

    #[test]
    fn merge_replaced_descarta_excesso_local_e_conta() {
        // Servidor encolheu (ex.: compact): não-system sem correspondente sai,
        // e a contagem volta em `dropped_local`.
        let mut msgs = vec![cm("user", "a"), cm("user", "b"), cm("system", "meio")];
        let server = vec![mi("user", "a")];
        assert_eq!(
            merge_messages(&mut msgs, &server),
            MergeResult::Replaced { dropped_local: 1 }
        );
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], cm("user", "a"));
        assert_eq!(msgs[1], cm("system", "meio"));
    }

    #[test]
    fn merge_replaced_avisa_quando_descarta_local() {
        // Apply injeta UMA notice system quando há descarte local; sem
        // descarte (dropped_local == 0), não polui o histórico.
        let mut app = TuiApp::new("sess_drop", "w", false);
        let mut created = serde_json::json!({});
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel("sess_drop".to_string());
        // Envio local que falhou ("a", nunca confirmado) + "x" confirmado;
        // o servidor só conhece "x" → "a" é descartada (dropped_local = 1).
        app.push_msg("user", "a");
        app.push_msg("user", "x");
        app.push_msg("system", "(splash)");
        let antes = app.messages.len();
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::Messages {
                sid: "sess_drop".into(),
                msgs: vec![mi("user", "x")],
                result: MergeResult::Replaced { dropped_local: 1 },
            },
        );
        let avisos = app
            .messages
            .iter()
            .filter(|m| m.text.contains("não confirmadas pelo servidor"))
            .count();
        assert_eq!(avisos, 1, "exatamente UMA notice de aviso");
        assert_eq!(app.messages.len(), antes, "2 locais viram 1 do servidor + notice system preservada + aviso");
        assert!(app
            .messages
            .iter()
            .any(|m| m.role == "system" && m.text == "(splash)"));

        // Replaced SEM descarte → sem aviso novo.
        let antes = app.messages.len();
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::Messages {
                sid: "sess_drop".into(),
                msgs: vec![mi("user", "c")],
                result: MergeResult::Replaced { dropped_local: 0 },
            },
        );
        assert_eq!(app.messages.len(), antes, "sem descarte → sem notice");
        assert_eq!(
            app.messages
                .iter()
                .filter(|m| m.text.contains("não confirmadas pelo servidor"))
                .count(),
            1,
            "continua só o aviso original"
        );
    }

    // ----- A-1: merge com FIO (envio com @arquivo) -----

    #[test]
    fn merge_wire_eco_expandido_e_unchanged_e_preserva_original() {
        // Local guarda ORIGINAL + fio (expandido); o servidor ecoa o
        // expandido → `Unchanged` (sem `Replaced`, sem `dropped_local`) e o
        // transcript NÃO troca o original pelo eco.
        let mut msgs = vec![
            cm("system", "splash"),
            cmw("user", "resuma @nota.md", "resuma CONTEUDO-EXPANDIDO"),
        ];
        let antes = msgs.clone();
        let server = vec![mi("user", "resuma CONTEUDO-EXPANDIDO")];
        assert_eq!(merge_messages(&mut msgs, &server), MergeResult::Unchanged);
        assert_eq!(msgs, antes, "original + fio preservados intocados");
    }

    #[test]
    fn merge_wire_prefixo_expandido_apenda_assistant() {
        // Eco do fio no prefixo + resposta nova no fim → `Appended`; a
        // local segue ORIGINAL (com o fio) e a resposta entra do servidor.
        let mut msgs = vec![cmw("user", "resuma @nota.md", "resuma CONTEUDO-EXPANDIDO")];
        let server = vec![
            mi("user", "resuma CONTEUDO-EXPANDIDO"),
            mi("assistant", "resumo pronto"),
        ];
        assert_eq!(merge_messages(&mut msgs, &server), MergeResult::Appended);
        assert_eq!(msgs[0], cmw("user", "resuma @nota.md", "resuma CONTEUDO-EXPANDIDO"));
        assert_eq!(msgs[1], cm("assistant", "resumo pronto"));
    }

    #[test]
    fn merge_sem_wire_servidor_expandido_diverge_e_replaced() {
        // SEM fio, o eco expandido diverge do local → `Replaced` como sempre
        // (documenta a diferença: é o fio que evita a troca do original).
        let mut msgs = vec![cm("user", "resuma @nota.md")];
        let server = vec![mi("user", "resuma CONTEUDO-EXPANDIDO")];
        assert_eq!(
            merge_messages(&mut msgs, &server),
            MergeResult::Replaced { dropped_local: 0 }
        );
        assert_eq!(msgs[0], cm("user", "resuma CONTEUDO-EXPANDIDO"));
    }

    #[test]
    fn merge_wire_divergencia_real_vira_replaced_e_limpa_fio() {
        // Servidor divergiu DE VERDADE do fio (ex.: pós-compact) → `Replaced`
        // real (comportamento mantido); a mensagem substituta vem SEM fio —
        // o texto dela já é do servidor (estável no próximo poll).
        let mut msgs = vec![cmw("user", "resuma @nota.md", "resuma FIO-ANTIGO")];
        let server = vec![mi("user", "resuma TEXTO-POS-COMPACT")];
        assert_eq!(
            merge_messages(&mut msgs, &server),
            MergeResult::Replaced { dropped_local: 0 }
        );
        assert_eq!(msgs[0], cm("user", "resuma TEXTO-POS-COMPACT"));
        assert!(msgs[0].wire.is_none(), "substituta do servidor não herda fio");
    }

    #[test]
    fn roundtrip_arquivo_push_com_wire_eco_expandido_mantem_original() {
        // Roundtrip @arquivo (A-1) pelo caminho REAL do apply: envio guarda
        // original+fio; o fetch traz o eco EXPANDIDO; o transcript mantém o
        // original, sem `Replaced` e SEM aviso de "não confirmadas".
        let mut app = TuiApp::new("sess_wire", "w", false);
        let mut created = serde_json::json!({});
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel("sess_wire".to_string());
        app.push_msg_with_wire(
            "user",
            "resuma @nota.md",
            Some("resuma CONTEUDO-EXPANDIDO".into()),
        );
        let antes = app.messages.clone();
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::Messages {
                sid: "sess_wire".into(),
                msgs: vec![mi("user", "resuma CONTEUDO-EXPANDIDO")],
                result: MergeResult::Appended,
            },
        );
        assert_eq!(app.messages, antes, "eco do servidor não altera o transcript");
        assert_eq!(app.messages.last().unwrap().text, "resuma @nota.md");
        assert_eq!(
            app.messages
                .iter()
                .filter(|m| m.text.contains("não confirmadas pelo servidor"))
                .count(),
            0,
            "sem aviso falso de mensagem local não confirmada"
        );
    }

    // ----- fluidez: poller, canal UiUpdate, coalescing (Fase 7) -----

    #[test]
    fn poll_interval_working_curto_ocioso_longo() {
        // Working: usa o config (default novo 300, config antigo 500), com
        // clamp mínimo de 100ms. Ocioso: cadence longo fixo.
        assert_eq!(poll_interval_ms(true, 300), 300);
        assert_eq!(poll_interval_ms(true, 500), 500, "config antiga respeitada");
        assert_eq!(poll_interval_ms(true, 50), 100, "clamp mínimo .max(100)");
        assert_eq!(poll_interval_ms(false, 300), IDLE_POLL_MS);
        assert_eq!(poll_interval_ms(false, 500), IDLE_POLL_MS);
        assert!(IDLE_POLL_MS >= 1000, "ocioso é ordens mais lento que working");
    }

    #[test]
    fn is_ctrl_c_event_so_control_c_qualquer_case() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let ctrl = |m: KeyModifiers, c: char| {
            Event::Key(KeyEvent::new(KeyCode::Char(c), m))
        };
        // Ctrl+C = saída (chega sempre minúsculo: ETX do terminal).
        assert!(is_ctrl_c_event(&ctrl(KeyModifiers::CONTROL, 'c')));
        // Outras teclas/modificadores NÃO.
        assert!(!is_ctrl_c_event(&ctrl(KeyModifiers::NONE, 'c')));
        assert!(!is_ctrl_c_event(&ctrl(KeyModifiers::SHIFT, 'c')));
        assert!(!is_ctrl_c_event(&ctrl(KeyModifiers::CONTROL, 'x')));
        // Eventos não-tecla NÃO (paste/mouse não são saída).
        assert!(!is_ctrl_c_event(&Event::Paste("c".into())));
    }

    #[test]
    fn poll_backoff_exponencial_cap_e_saturacao() {
        // Sem falhas → cadência base intacta (working ou idle).
        assert_eq!(poll_backoff_ms(1500, 0), 1500);
        assert_eq!(poll_backoff_ms(300, 0), 300);
        // Cada falha consecutiva DOBRA a próxima espera.
        assert_eq!(poll_backoff_ms(1500, 1), 3000);
        assert_eq!(poll_backoff_ms(1500, 2), 6000);
        assert_eq!(poll_backoff_ms(1500, 3), 12000);
        // Teto: 1500×16 = 24000 > 15s → cap (e nunca passa dele).
        assert_eq!(poll_backoff_ms(1500, 4), POLL_BACKOFF_CAP_MS);
        assert_eq!(poll_backoff_ms(1500, 100), POLL_BACKOFF_CAP_MS);
        assert_eq!(
            poll_backoff_ms(1500, u32::MAX),
            POLL_BACKOFF_CAP_MS,
            "saturação u32 sem overflow no shift"
        );
        // Base working (curta) também satura no teto.
        assert_eq!(poll_backoff_ms(100, 8), POLL_BACKOFF_CAP_MS);
        assert_eq!(poll_backoff_ms(100, 1), 200);
    }

    #[test]
    fn poll_backoff_independe_do_streak_de_morte() {
        // Erro de RPC (servidor VIVO respondendo erro) NÃO acumula o streak
        // de morte, MAS alonga o backoff: servidor em apuros é pollado mais
        // devagar sem ser declarado morto.
        let rpc = || {
            session::SessionError::Runtime(crate::runtime::RuntimeError::Rpc(
                crate::rpc::RpcError::Server {
                    code: -32603,
                    message: "erro interno".into(),
                },
            ))
        };
        let mut streak = 0u32;
        let mut fails = 0u32;
        for _ in 0..5 {
            streak = poll_fail_streak(streak, &rpc());
            fails = fails.saturating_add(1); // mesma contagem do poller
        }
        assert_eq!(streak, 0, "RPC não é morte de runtime");
        assert_eq!(
            poll_backoff_ms(IDLE_POLL_MS, fails),
            POLL_BACKOFF_CAP_MS,
            "5 falhas seguidas → espera no teto (15s)"
        );
        // Sucesso reseta: volta à cadência base.
        assert_eq!(poll_backoff_ms(IDLE_POLL_MS, 0), IDLE_POLL_MS);
    }

    // ----- Fase V4-1 (startup instantâneo): gate do poller -----

    #[test]
    fn poll_should_fetch_so_com_sid_real() {
        // O watch sid começa no placeholder "" (TUI já visível, sessão não):
        // nenhum fetch até o 1º NewSession trocar o valor.
        assert!(!poll_should_fetch(""), "placeholder segura o poller");
        assert!(poll_should_fetch("sess_abc"), "sid real libera o fetch");
    }

    #[test]
    fn diff_lists_mesma_semantica_do_merge() {
        let a = vec![mi("user", "oi")];
        assert_eq!(diff_lists(&a, &a), MergeResult::Unchanged);
        let ab = vec![mi("user", "oi"), mi("assistant", "olá")];
        assert_eq!(diff_lists(&a, &ab), MergeResult::Appended);
        let b = vec![mi("user", "mudou")];
        assert_eq!(
            diff_lists(&a, &b),
            MergeResult::Replaced { dropped_local: 0 }
        );
        // Encolheu (compact) → Replaced.
        assert_eq!(
            diff_lists(&ab, &a),
            MergeResult::Replaced { dropped_local: 0 }
        );
        // Vazio → vazio é Unchanged (nada despachado por tick).
        assert_eq!(diff_lists(&[], &[]), MergeResult::Unchanged);
        // Item completo diverge também por kind (reasoning novo).
        let r = vec![session::MsgItem {
            role: "user".into(),
            text: "oi".into(),
            kind: crate::session::MsgKind::Reasoning,
        }];
        assert_eq!(
            diff_lists(&a, &r),
            MergeResult::Replaced { dropped_local: 0 }
        );
    }

    #[test]
    fn uiupdate_aplicado_na_task_da_ui_sem_runtime() {
        let mut app = TuiApp::new("sess_t", "w", false);
        let mut created = serde_json::json!({});
        let (sid_tx, sid_rx) = tokio::sync::watch::channel("sess_t".to_string());

        apply_ui_update(&mut app, &mut created, &sid_tx, UiUpdate::Notice("oi".into()));
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, "system");

        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::Usage {
                usage: session::Usage {
                    total_tokens: 10,
                    input_tokens: 7,
                    output_tokens: 3,
                    model_request_count: 2,
                    ..Default::default()
                },
                notice: true,
            },
        );
        assert_eq!(app.tokens_in, 7);
        assert_eq!(app.tokens_out, 3);
        assert!(app.messages.last().unwrap().text.contains("total=10"));

        // Messages: bump de rev quando muda, NENHUM bump quando Unchanged.
        let msgs = vec![mi("user", "oi")];
        let rev0 = app.messages_rev;
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::Messages {
                sid: "sess_t".into(),
                msgs: msgs.clone(),
                result: MergeResult::Appended,
            },
        );
        assert_eq!(app.messages_rev, rev0 + 1);
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::Messages {
                sid: "sess_t".into(),
                msgs,
                result: MergeResult::Unchanged,
            },
        );
        assert_eq!(app.messages_rev, rev0 + 1, "Unchanged não invalida cache");
        // Update de sessão ANTIGA é descartado (corrida com Ctrl+N).
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::Messages {
                sid: "sess_velha".into(),
                msgs: vec![mi("user", "fantasma")],
                result: MergeResult::Appended,
            },
        );
        assert!(!app.messages.iter().any(|m| m.text == "fantasma"));

        // NewSession troca sessão, limpa histórico e sinaliza o poller.
        let created_new = serde_json::json!({
            "session": {"sessionId": "sess_n"},
            "projection": {"contextWindow": 10, "contextUsed": 5},
            "todos": [
                {"content": "ler plano", "status": "completed"},
                {"content": "implementar", "status": "in-progress"}
            ]
        });
        app.runtime_dead = true; // trava de sessão morta é limpa pela sessão nova
        app.session_ready = false; // boot da 1ª sessão (Fase V4-1)
        app.status = "conectando…".to_string();
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::NewSession {
                created: created_new,
                sid: "sess_n".into(),
                model: "zai/glm-5.3-Flash".into(),
            },
        );
        assert_eq!(app.session_id, "sess_n");
        assert_eq!(created["session"]["sessionId"], "sess_n");
        assert_eq!(app.ctx, session::ContextUsage { window: 10, used: 5 });
        assert!(app.follow, "sessão nova recomeça grudada no fim");
        assert_eq!(app.scroll, 0);
        assert_eq!(*sid_rx.borrow(), "sess_n", "poller troca de sessão");
        assert!(app.messages.len() == 1, "histórico limpo + notice");
        assert!(!app.runtime_dead, "sessão nova limpa a trava de runtime morto");
        // Fase V4-1: o NewSession do boot destrava Enter/poller e troca o
        // status; Fase V4-2: todos do create + modelo efetivo.
        assert!(app.session_ready, "boot completo → session_ready");
        assert_eq!(app.status, "pronto");
        assert_eq!(app.model, "zai/glm-5.3-Flash");
        assert_eq!(app.todos.len(), 2);
        assert_eq!(app.todos[0], session::TodoItem {
            content: "ler plano".into(),
            status: session::TodoStatus::Completed,
        });

        // SessionEvent não toca no estado (só marca dirty no loop).
        let antes = app.messages.clone();
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::SessionEvent {
                method: "stream.chunk".into(),
            },
        );
        assert_eq!(app.messages, antes);
    }

    // ----- Fase V5-2: picker de sessões + troca de sessão (resume) -----

    #[test]
    fn sessions_loaded_alimenta_picker_e_erro_fecha_com_notice() {
        let mut app = TuiApp::new("sess_t", "w", false);
        let mut created = serde_json::json!({});
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel(String::new());

        // Picker aberto + lista chegando: itens entram e o status troca.
        app.open_session_picker();
        let v = serde_json::json!({"sessions": [
            {"sessionId": "sess_a", "title": "plano", "status": "active", "createdAt": "2026-09-08"}
        ]});
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::SessionsLoaded { res: Ok(v) },
        );
        let p = app.picker.as_ref().expect("picker segue aberto");
        assert_eq!(p.len(), 1);
        assert_eq!(p.selected_id().as_deref(), Some("sess_a"));
        assert!(app.status.contains("↑/↓"), "{}", app.status);

        // Falha do fetch: picker FECHA (não fica preso em "buscando…") +
        // notice honesta; status volta ao repouso.
        app.open_session_picker();
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::SessionsLoaded {
                res: Err("daemon sumiu".into()),
            },
        );
        assert!(app.picker.is_none());
        assert_eq!(app.status, "pronto");
        assert!(app.messages.last().unwrap().text.contains("sessions falhou"));

        // Lista chegando DEPOIS do Esc: descarte silencioso (picker segue
        // fechado, sem notice inventada).
        let v2 = serde_json::json!({"sessions": [{"sessionId": "sess_b"}]});
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::SessionsLoaded { res: Ok(v2) },
        );
        assert!(app.picker.is_none());
    }

    #[test]
    fn resumed_session_reusa_a_projecao_do_new_session_com_status_de_resume() {
        let mut app = TuiApp::new("sess_atual", "w", false);
        let mut created = serde_json::json!({});
        let (sid_tx, sid_rx) = tokio::sync::watch::channel("sess_atual".to_string());
        // Estado "sujo" da sessão anterior: o switch tem que limpar TUDO que
        // o NewSession limpa (mesma projeção, caminho único).
        app.session_ready = false;
        app.booting = true;
        app.runtime_dead = true;
        app.working = false;
        app.push_msg("user", "mensagem da sessão antiga");
        app.scroll = 7;
        app.follow = false;
        app.open_search();
        app.todos = vec![session::TodoItem {
            content: "todo velho".into(),
            status: session::TodoStatus::Completed,
        }];

        let resumed = serde_json::json!({
            "session": {"sessionId": "sess_retomada"},
            "settings": {"model": {"current": {"providerId": "zai", "modelId": "glm-5.3-Flash"}}},
        });
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::ResumedSession {
                resumed,
                sid: "sess_retomada".into(),
                model: "zai/glm-5.3-Flash".into(),
                quente: false, // transporte Embedded → leitura garantida (R1)
            },
        );
        assert_eq!(app.session_id, "sess_retomada");
        assert_eq!(created["session"]["sessionId"], "sess_retomada");
        assert_eq!(*sid_rx.borrow(), "sess_retomada", "poller troca de alvo");
        assert!(app.session_ready, "resume também destrava Enter/poller");
        assert!(!app.booting, "single-flight destravado pela resposta");
        assert!(!app.runtime_dead, "switch limpa a trava de runtime morto");
        assert_eq!(
            app.messages.len(),
            1,
            "transcript zerado (o poller busca o novo) + notice do resume"
        );
        assert_eq!(app.scroll, 0);
        assert!(app.follow, "recomeça grudado no fim");
        assert!(app.search.is_none(), "busca da sessão antiga descartada");
        assert!(app.todos.is_empty(), "todos da sessão anterior NÃO vazam");
        assert_eq!(app.model, "zai/glm-5.3-Flash", "modelo vem do result do resume");
        // Status honesto pelo R1 condicional: Embedded → leitura.
        assert_eq!(app.status, "sessão retomada: sess_ret (leitura — R1)");
        assert!(app
            .messages
            .last()
            .unwrap()
            .text
            .contains("(leitura — R1)"));
        assert!(app.messages.last().unwrap().text.contains("sessão retomada"));

        // Daemon (quente=true): status aponta envio possível — sem inventar
        // certeza (sessão fria aparece como -32031 no turno, como sempre).
        app.open_search();
        let resumed2 = serde_json::json!({"session": {"sessionId": "sess_q"}});
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::ResumedSession {
                resumed: resumed2,
                sid: "sess_quente".into(),
                model: String::new(),
                quente: true,
            },
        );
        assert_eq!(app.status, "sessão retomada: sess_que (quente — pode enviar)");
        assert!(app.search.is_none());
    }

    #[test]
    fn resumed_session_sid_novo_do_servidor_vence_fallback_e_id_no_status() {
        // (Cobertura do spawn em apply: o `extract_session_id` é a fonte da
        // verdade; aqui valida a formatação do id8 curto no status.)
        assert_eq!(session::sid_short8("abcdefgh resto"), "abcdefgh");
        let mut app = TuiApp::new("s", "w", false);
        let mut created = serde_json::json!({});
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel(String::new());
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::ResumedSession {
                resumed: serde_json::json!({}),
                sid: "abcdefgh-muito-mais".into(),
                model: String::new(),
                quente: false,
            },
        );
        assert_eq!(app.session_id, "abcdefgh-muito-mais");
        assert_eq!(app.status, "sessão retomada: abcdefgh (leitura — R1)");
    }

    #[test]
    fn troca_de_sessao_descarta_fila_e_fecha_overlay_com_notice_unica() {
        // M-1: itens enfileirados pertencem à sessão abandonada — a troca
        // zera a fila, fecha o overlay aberto e emite UMA notice com a
        // contagem; nada vaza para a sessão nova.
        let mut app = TuiApp::new("sess_atual", "w", false);
        let mut created = serde_json::json!({});
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel("sess_atual".to_string());
        app.queue_push("pendente 1");
        app.queue_push("pendente 2");
        app.overlay = Some(tui::Overlay::Todos);
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::ResumedSession {
                resumed: serde_json::json!({}),
                sid: "sess_nova".into(),
                model: String::new(),
                quente: false,
            },
        );
        assert!(app.queue.is_empty(), "fila da sessão antiga descartada");
        assert!(app.overlay.is_none(), "overlay da sessão antiga fechado");
        assert_eq!(
            app.messages
                .iter()
                .filter(|m| m.text.contains("fila descartada na troca de sessão"))
                .count(),
            1,
            "exatamente UMA notice de fila descartada"
        );
        assert!(app
            .messages
            .iter()
            .any(|m| m.text == "fila descartada na troca de sessão (2 mensagem/ns)"));
    }

    #[test]
    fn troca_de_sessao_sem_fila_nao_emite_notice_de_fila() {
        // M-1: fila vazia → troca silenciosa quanto à fila (só a notice da
        // própria troca entra no transcript).
        let mut app = TuiApp::new("sess_atual", "w", false);
        let mut created = serde_json::json!({});
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel("sess_atual".to_string());
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::ResumedSession {
                resumed: serde_json::json!({}),
                sid: "sess_nova".into(),
                model: String::new(),
                quente: false,
            },
        );
        assert!(app.queue.is_empty());
        assert!(
            !app.messages
                .iter()
                .any(|m| m.text.contains("fila descartada")),
            "sem fila → sem notice de fila"
        );
    }

    /// KeyCtx de teste SEM runtime (`rt: None`) — os braços de RPC tratam o
    /// vazio com notice; o gate do picker e a navegação são 100% locais.
    fn kctx_sem_runtime<'a>(
        app: &'a mut TuiApp,
        created: &'a mut Value,
        send_task: &'a mut Option<TurnJoin>,
        last_diff: &'a mut Option<WorkspaceDiff>,
        boot: &'a SessionBoot,
        working_tx: &'a tokio::sync::watch::Sender<bool>,
        ui_tx: &'a UiTx,
        poll_wake: &'a std::sync::Arc<tokio::sync::Notify>,
    ) -> KeyCtx<'a> {
        KeyCtx {
            app,
            created,
            rt: None,
            boot,
            ws: "C:/ws-teste",
            send_task,
            working_tx,
            ui_tx,
            poll_wake,
            last_diff,
        }
    }

    #[test]
    fn picker_gate_teclas_navegam_esc_fecha_e_enter_sem_rt_notica() {
        use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
        use std::sync::atomic::AtomicBool;
        let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let (working_tx, _working_rx) = tokio::sync::watch::channel(false);
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        use clap::Parser as _;
        let boot = SessionBoot {
            cli: Cli::parse_from(["zcode-cli"]),
            cfg: config::FileConfig::default(),
            rt_slot: std::sync::Arc::new(std::sync::OnceLock::new()),
            quit: std::sync::Arc::new(AtomicBool::new(false)),
            poll_wake: wake.clone(),
        };
        let mut app = TuiApp::new("sess_t", "w", false);
        app.mark_session_ready();
        app.open_session_picker();
        app.fill_session_picker(vec![
            session::SessionRow {
                id: "sess_a".into(),
                title: "a".into(),
                status: "active".into(),
                created: "2026-09-08".into(),
            },
            session::SessionRow {
                id: "sess_b".into(),
                title: "b".into(),
                status: "active".into(),
                created: "2026-09-08".into(),
            },
        ]);
        let mut created = serde_json::json!({});
        let mut send_task: Option<TurnJoin> = None;
        let mut last_diff: Option<WorkspaceDiff> = None;

        // ↑ no topo: CLAMP (fica); ↓ desce. Teclas do picker NÃO vazam para
        // o input e a navegação NÃO usa o recall de histórico.
        {
            let mut ctx = kctx_sem_runtime(
                &mut app,
                &mut created,
                &mut send_task,
                &mut last_diff,
                &boot,
                &working_tx,
                &ui_tx,
                &wake,
            );
            assert!(handle_key_event(&mut ctx, tecla(KeyCode::Up, KeyModifiers::NONE, KeyEventKind::Press)));
            assert!(handle_key_event(&mut ctx, tecla(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Press)));
            assert!(handle_key_event(&mut ctx, tecla(KeyCode::Char('x'), KeyModifiers::NONE, KeyEventKind::Press)));
        }
        assert_eq!(app.picker.as_ref().unwrap().selected, 1);
        assert!(app.input.is_empty(), "char com picker aberto não digita");
        // Esc fecha (sem tocar no transcript).
        {
            let mut ctx = kctx_sem_runtime(
                &mut app,
                &mut created,
                &mut send_task,
                &mut last_diff,
                &boot,
                &working_tx,
                &ui_tx,
                &wake,
            );
            assert!(handle_key_event(&mut ctx, tecla(KeyCode::Esc, KeyModifiers::NONE, KeyEventKind::Press)));
        }
        assert!(app.picker.is_none());
        assert_eq!(app.status, "pronto");

        // Enter com picker aberto toma o id e FECHA o picker; sem runtime o
        // braço defensivo emite a notice de sessão pendente (mesma do Ctrl+U).
        app.open_session_picker();
        app.fill_session_picker(vec![session::SessionRow {
            id: "sess_alvo".into(),
            title: "alvo".into(),
            status: "active".into(),
            created: "2026-09-08".into(),
        }]);
        {
            let mut ctx = kctx_sem_runtime(
                &mut app,
                &mut created,
                &mut send_task,
                &mut last_diff,
                &boot,
                &working_tx,
                &ui_tx,
                &wake,
            );
            assert!(handle_key_event(&mut ctx, tecla(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press)));
        }
        assert!(app.picker.is_none(), "Enter fecha o picker");
        assert!(app
            .messages
            .iter()
            .any(|m| m.text.contains("(retomando sessão sess_alv…)")));
        assert!(app
            .messages
            .last()
            .unwrap()
            .text
            .contains("Ctrl+N p/ tentar de novo"));

        // Ctrl+C SEMPRE passa: fecha o picker E arma o fluxo de saída (o
        // 2º Ctrl+C sairia — invariante dos overlays preservado).
        app.open_session_picker();
        {
            let mut ctx = kctx_sem_runtime(
                &mut app,
                &mut created,
                &mut send_task,
                &mut last_diff,
                &boot,
                &working_tx,
                &ui_tx,
                &wake,
            );
            use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
            let ctrlc = Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: KeyEventState::empty(),
            });
            assert!(handle_key_event(&mut ctx, ctrlc));
        }
        assert!(app.picker.is_none(), "Ctrl+C fecha o picker");
        assert!(app.ctrlc_once, "e o fluxo de saída segue armado");
    }

    // ----- Fase 8 (TUI visual): runtime morto, poll streak, diff notice -----

    #[test]
    fn runtime_exited_info_mapeia_só_exited() {
        // SessionError::Runtime(#[from] Exited(code, tail)) → banner (code, tail).
        let e = session::SessionError::Runtime(crate::runtime::RuntimeError::Exited(
            Some(1),
            "boom\n".into(),
        ));
        assert_eq!(
            runtime_exited_info(&e),
            Some((Some(1), "boom\n".to_string()))
        );
        // Exit sem código (signal) preserva None.
        let e2 = session::SessionError::Runtime(crate::runtime::RuntimeError::Exited(
            None,
            String::new(),
        ));
        assert_eq!(runtime_exited_info(&e2), Some((None, String::new())));
        // Outros erros NÃO são runtime morto (texto atual preservado).
        assert_eq!(runtime_exited_info(&session::SessionError::Protocol("timeout (300s) sem resposta do assistant".into())), None);
        assert_eq!(
            runtime_exited_info(&session::SessionError::Runtime(
                crate::runtime::RuntimeError::Timeout(30, "session/send".into())
            )),
            None
        );
        // Timeout do runtime → é timeout (não acumula streak de morte).
        assert!(poll_err_is_timeout(&session::SessionError::Runtime(
            crate::runtime::RuntimeError::Timeout(30, "session/messages".into())
        )));
        assert!(!poll_err_is_timeout(&e));
    }

    #[test]
    fn poll_fail_streak_so_processo_morto_ou_io_local_acumula() {
        let exited = || {
            session::SessionError::Runtime(crate::runtime::RuntimeError::Exited(
                Some(1),
                "boom".into(),
            ))
        };
        // 3× Exited seguidas → streak chega ao limiar (runtime morto).
        let s = poll_fail_streak(0, &exited());
        let s = poll_fail_streak(s, &exited());
        assert_eq!(
            poll_fail_streak(s, &exited()),
            POLL_DEAD_STREAK,
            "3 quedas de processo seguidas declaram runtime morto"
        );
        // IO local (pipe quebrado) também acumula junto com Exited.
        let io = || {
            session::SessionError::Runtime(crate::runtime::RuntimeError::Io(
                "pipe quebrado".into(),
            ))
        };
        let s = poll_fail_streak(poll_fail_streak(0, &exited()), &io());
        assert_eq!(
            poll_fail_streak(s, &exited()),
            POLL_DEAD_STREAK,
            "Exited + Io local somam quedas consecutivas"
        );
        // 3× erro de servidor (RPC/protocolo) → NÃO declara runtime morto
        // (Node vivo respondendo erro zera o streak).
        let rpc = || {
            session::SessionError::Runtime(crate::runtime::RuntimeError::Rpc(
                crate::rpc::RpcError::Server {
                    code: -32601,
                    message: "método ausente".into(),
                },
            ))
        };
        let s = poll_fail_streak(0, &rpc());
        let s = poll_fail_streak(s, &rpc());
        assert_eq!(
            poll_fail_streak(s, &rpc()),
            0,
            "erro de RPC do servidor não acumula o streak"
        );
        assert_eq!(
            poll_fail_streak(0, &session::SessionError::Protocol("boom".into())),
            0,
            "erro de protocolo não acumula o streak"
        );
        // Timeout ENTRE quedas reseta o streak (comportamento mantido).
        let s = poll_fail_streak(poll_fail_streak(0, &exited()), &exited());
        let s = poll_fail_streak(
            s,
            &session::SessionError::Runtime(crate::runtime::RuntimeError::Timeout(
                30,
                "session/messages".into(),
            )),
        );
        assert_eq!(s, 0, "timeout reseta o streak");
        // Saturação: sem overflow em streak longo.
        assert_eq!(poll_fail_streak(u32::MAX, &exited()), u32::MAX);
    }

    #[test]
    fn git_stat_summary_e_format_diff_notice() {
        // Última linha do `git diff --stat` real.
        let stat = " src/a.rs | 12 ++++++---\n src/b.rs | 2 +-\n 2 files changed, 9 insertions(+), 3 deletions(-)";
        assert_eq!(git_stat_summary(stat).unwrap(), "+9 −3");
        // Só inserções / só deleções.
        assert_eq!(git_stat_summary(" 1 file changed, 1 insertion(+)").unwrap(), "+1");
        assert_eq!(git_stat_summary(" 1 file changed, 2 deletions(-)").unwrap(), "−2");
        assert_eq!(git_stat_summary(""), None);
        assert_eq!(git_stat_summary(" 1 file changed").is_none(), true);

        // Notice completa: editou + git (formato de referência do headless).
        let wd = WorkspaceDiff {
            created: vec!["novo.rs".into()],
            modified: vec!["a.rs".into(), "b.rs".into()],
        };
        assert_eq!(
            format_diff_notice(Some(&wd), Some(stat)).unwrap(),
            "editou: a.rs, b.rs, novo.rs — git: +9 −3"
        );
        // Só git / só snapshot / nada.
        assert_eq!(format_diff_notice(None, Some(stat)).unwrap(), "git: +9 −3");
        assert_eq!(
            format_diff_notice(Some(&WorkspaceDiff::default()), Some(stat)).unwrap(),
            "git: +9 −3"
        );
        let so_snap = WorkspaceDiff { created: vec!["x".into()], modified: vec![] };
        assert_eq!(
            format_diff_notice(Some(&so_snap), None).unwrap(),
            "editou: x"
        );
        assert_eq!(format_diff_notice(None, None), None);
        assert_eq!(
            format_diff_notice(Some(&WorkspaceDiff::default()), None),
            None
        );
        // Stat sem resumo parseável → 1ª linha truncada como fallback.
        assert_eq!(
            format_diff_notice(None, Some("src/a.rs | 12 +++++")).unwrap(),
            "git: src/a.rs | 12 +++++"
        );
        // Lista grande é cortada em 5 nomes com contador.
        let mut muitos: Vec<String> = (0..8).map(|i| format!("f{i}.rs")).collect();
        muitos.sort();
        let wd_muitos = WorkspaceDiff { created: muitos, modified: vec![] };
        let n = format_diff_notice(Some(&wd_muitos), None).unwrap();
        assert!(n.contains("f0.rs") && n.contains("(+3)"), "{n}");
    }

    #[test]
    fn uiupdate_runtime_dead_aplica_trava_e_banner() {
        let mut app = TuiApp::new("sess_dead", "w", false);
        let mut created = serde_json::json!({});
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel("sess_dead".to_string());
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::RuntimeDead {
                notice: render::runtime_dead_banner(Some(1), "erro fatal\nlinha2\n"),
            },
        );
        assert!(app.runtime_dead, "trava de turnos ligada");
        assert_eq!(app.status, "runtime morto");
        let ultima = app.messages.last().unwrap();
        assert_eq!(ultima.role, "system");
        assert!(ultima.text.contains("exit 1"));
        assert!(ultima.text.contains("/exit e reabra"));
        // A trava fica ligada até uma sessão nova (não há auto-reset).
        assert!(app.runtime_dead);
    }

    #[test]
    fn coalescing_drena_lote_em_um_draw() {
        let (tx, mut rx): (UiTx, UiRx) = tokio::sync::mpsc::unbounded_channel();
        for i in 0..5 {
            tx.send(UiUpdate::Notice(format!("n{i}"))).unwrap();
        }
        drop(tx);
        let mut app = TuiApp::new("s", "w", false);
        let mut created = serde_json::json!({});
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel("s".to_string());
        // 5 updates enfileirados → 1 dreno (um único draw no loop real).
        assert!(apply_pending_updates(&mut rx, &mut app, &mut created, &sid_tx));
        assert_eq!(app.messages.len(), 5);
        // Fila vazia → sem dirty (nada a desenhar).
        assert!(!apply_pending_updates(&mut rx, &mut app, &mut created, &sid_tx));
    }

    // ----- overlays (/context, /usage): last_usage guarda o usage completo -----

    #[test]
    fn uiupdate_usage_guarda_last_usage_e_newsessao_reseta() {
        let mut app = TuiApp::new("sess_u", "w", false);
        let mut created = serde_json::json!({});
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel("sess_u".to_string());
        assert!(app.last_usage.is_none(), "começa sem usage (overlay usa —)");
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::Usage {
                usage: session::Usage {
                    total_tokens: 90,
                    input_tokens: 60,
                    output_tokens: 30,
                    cache_read_tokens: 40,
                    model_request_count: 2,
                    ..Default::default()
                },
                notice: false,
            },
        );
        // In/out continuam na statusbar; o COMPLETO vai p/ os overlays.
        assert_eq!((app.tokens_in, app.tokens_out), (60, 30));
        let u = app.last_usage.as_ref().expect("usage completo guardado");
        assert_eq!(
            (u.total_tokens, u.reasoning_tokens, u.cache_read_tokens, u.model_request_count),
            (90, 0, 40, 2)
        );
        assert_eq!(tui::cache_hit(u), 40.0);
        // Ctrl+N: usage da sessão anterior não vale para a nova.
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::NewSession {
                created: serde_json::json!({"session": {"sessionId": "sess_n2"}}),
                sid: "sess_n2".into(),
                // model vazio: mantém o atual (não inventa default).
                model: String::new(),
            },
        );
        assert!(app.last_usage.is_none(), "reset junto com last_stats");
        assert!(app.last_stats.is_none());
    }

    // ----- paste por timing: Enter colado vira newline (Windows nativo) -----

    use crossterm::event::{KeyEvent, KeyModifiers};

    fn tecla(code: KeyCode, mods: KeyModifiers, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent::new_with_kind(code, mods, kind))
    }

    #[test]
    fn enfileirar_nao_registra_historico_nem_troca_status() {
        // A-2/B-1: Enter durante `working` SÓ enfileira — o histórico de
        // prompts registra no CONSUMO real (`spawn_turn`, mesmo contrato do
        // envio) e o status segue "working…" (o badge `[fila: N]` informa).
        use std::sync::atomic::AtomicBool;
        let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let (working_tx, _working_rx) = tokio::sync::watch::channel(false);
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        use clap::Parser as _;
        let boot = SessionBoot {
            cli: Cli::parse_from(["zcode-cli"]),
            cfg: config::FileConfig::default(),
            rt_slot: std::sync::Arc::new(std::sync::OnceLock::new()),
            quit: std::sync::Arc::new(AtomicBool::new(false)),
            poll_wake: wake.clone(),
        };
        let mut app = TuiApp::new("sess_fila", "w", false);
        app.mark_session_ready();
        app.working = true;
        app.status = "working…".to_string();
        let mut created = serde_json::json!({});
        let mut send_task: Option<TurnJoin> = None;
        let mut last_diff: Option<WorkspaceDiff> = None;

        // Enfileira 2: buffer limpa, histórico intacto, status intacto.
        for texto in ["primeira", "segunda"] {
            app.input = texto.into();
            app.cursor = app.input_len();
            let mut ctx = kctx_sem_runtime(
                &mut app,
                &mut created,
                &mut send_task,
                &mut last_diff,
                &boot,
                &working_tx,
                &ui_tx,
                &wake,
            );
            assert!(handle_key_event(
                &mut ctx,
                tecla(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press)
            ));
        }
        assert_eq!(app.queue.len(), 2, "ambas enfileiradas");
        assert!(app.input.is_empty(), "buffer limpo ao enfileirar");
        assert!(
            app.prompt_history.is_empty(),
            "A-2: enfileirar NÃO registra histórico (só o consumo real)"
        );
        assert_eq!(app.status, "working…", "B-1: sem status 'na fila: N'");

        // Consumo simulado (contrato do `spawn_turn`: pop + remember) —
        // cada prompt aparece EXATAMENTE 1× no histórico.
        while let Some(texto) = app.next_queued() {
            app.remember_prompt(&texto);
        }
        assert_eq!(app.prompt_history, vec!["primeira", "segunda"]);
    }

    #[test]
    fn paste_enter_rapido_converte_em_newline() {
        // Enter <20ms após outra tecla = paste sintetizado → Paste("\n").
        let enter = tecla(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press);
        assert!(should_convert_enter(0, &enter));
        assert!(should_convert_enter(19, &enter), "19ms ainda é colagem");
        // Gap ≥20ms = tecla humana/auto-repeat → Enter REAL (envia o turno).
        assert!(!should_convert_enter(20, &enter));
        assert!(!should_convert_enter(50, &enter));
        // Via wrapper com Instant real: recente converte, antigo não.
        let recente = std::time::Instant::now() - std::time::Duration::from_millis(5);
        assert_eq!(
            enter_to_newline(Some(recente), &enter),
            Some(Event::Paste("\n".into()))
        );
    }

    #[test]
    fn paste_enter_com_modificador_ou_release_nunca_converte() {
        // SHIFT/CONTROL+Enter são atalho (TuiKey::Newline etc.), nunca paste —
        // mesmo com gap ~0.
        for mods in [KeyModifiers::SHIFT, KeyModifiers::CONTROL] {
            let ev = tecla(KeyCode::Enter, mods, KeyEventKind::Press);
            assert!(!should_convert_enter(0, &ev), "{mods:?} não converte");
        }
        // Release não é paste (só Press dispara a conversão).
        let release = tecla(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release);
        assert!(!should_convert_enter(0, &release));
        // Wrapper: Enter lento (≥20ms) não vira newline.
        let antigo = std::time::Instant::now() - std::time::Duration::from_millis(21);
        let enter = tecla(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press);
        assert_eq!(enter_to_newline(Some(antigo), &enter), None);
    }

    #[test]
    fn paste_mouse_resize_e_primeiro_evento_nao_convertem() {
        let recente = std::time::Instant::now() - std::time::Duration::from_millis(1);
        // Mouse/Resize não são Key: nunca convertem (e a thread só atualiza o
        // relógio no braço de Key — não "colam" o próximo Enter).
        let mouse = Event::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(enter_to_newline(Some(recente), &mouse), None);
        assert_eq!(enter_to_newline(Some(recente), &Event::Resize(80, 24)), None);
        // 1º evento da thread (sem tecla anterior) → nunca converte.
        let enter = tecla(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press);
        assert_eq!(enter_to_newline(None, &enter), None);
    }

    // ----- A-3 (V3)/I-2 (V4): gate do panic hook por ThreadId + JoinError -----

    /// Serializa os testes do gate: o estado agora é GLOBAL (um único
    /// ThreadId gravado), então testes paralelos que setam/limpom a marca
    /// teriam corrida entre si sem este lock.
    static GATE_UI_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn ui_thread_flag_e_guard_limpa_no_drop() {
        let _s = GATE_UI_TESTS.lock().unwrap_or_else(|p| p.into_inner());
        // Fora da UI (default): hook NÃO restauraria o terminal.
        assert!(!is_ui_thread(), "thread de teste não é a da UI");
        // Guard marca no enter e limpa no Drop (fim normal OU unwind de
        // panic/erro antecipado — o mecanismo é o mesmo Drop).
        {
            let _g = enter_ui_thread();
            assert!(is_ui_thread(), "dentro do run_tui o ThreadId está gravado");
        }
        assert!(!is_ui_thread(), "Drop do guard limpa o ThreadId");
        // Reentrada funciona (Ctrl+N não reentra, mas o guard é reutilizável).
        {
            let _g = enter_ui_thread();
            assert!(is_ui_thread());
        }
        assert!(!is_ui_thread());
    }

    // ----- Guarda de TTY (mata a TUI fantasma): critério puro + erro -----

    #[test]
    fn guarda_tty_criterio_exige_stdin_e_stdout() {
        // TTY genuíno: ambos terminais → TUI autorizada.
        assert!(ensure_tui_tty(true, true).is_ok());
        // Qualquer redirect mata. O fantasma comprovado (`tui < /dev/null
        // > arquivo`) é o caso (false, false).
        assert!(ensure_tui_tty(false, false).is_err());
        assert!(ensure_tui_tty(true, false).is_err(), "stdout em arquivo/pipe: sem TUI");
        assert!(ensure_tui_tty(false, true).is_err(), "stdin fora de TTY: sem TUI");
    }

    #[test]
    fn guarda_tty_erro_claro_e_exit_1() {
        let e = ensure_tui_tty(false, false).unwrap_err();
        assert!(matches!(e, CmdError::Io(_)), "erro de Io → exit code 1");
        assert_eq!(exit_code(&e), 1);
        let msg = e.to_string();
        assert!(msg.contains("TTY"), "{msg}");
        assert!(msg.contains("-p/--json"), "aponta a alternativa headless: {msg}");
    }

    #[test]
    fn ui_thread_outra_thread_nao_herde_gate() {
        // I-2 da V4: com o ThreadId gravado na thread PRINCIPAL (a da task
        // da UI), uma OUTRA thread física não é reconhecida como UI — o
        // thread_local antigo valia por thread e mentia sob migração de
        // task; a comparação por ThreadId é à prova disso.
        let _s = GATE_UI_TESTS.lock().unwrap_or_else(|p| p.into_inner());
        let _g = enter_ui_thread();
        assert!(is_ui_thread(), "thread principal gravou o ThreadId dela");
        // Na thread secundária (paralela à marca gravada): nunca é a UI.
        let filho = std::thread::spawn(|| is_ui_thread());
        assert!(!filho.join().unwrap(), "outra thread física ≠ UI");
        // E ainda é a UI aqui, após o join (o estado não foi tocado).
        assert!(is_ui_thread());
    }

    #[test]
    fn turn_join_error_notice_shape() {
        let n = turn_join_error_notice();
        assert!(n.starts_with("⚠ turno terminou inesperadamente"), "{n}");
        assert!(n.contains("erro interno"), "{n}");
        assert!(n.contains("detalhes no log"), "{n}");
        assert!(n.contains("zcode-cli doctor"), "{n}");
        assert!(n.lines().count() == 1, "uma linha só (notice de transcript)");
    }

    // ----- B-3 (V4): single-flight do boot (TuiApp.booting) -----

    #[test]
    fn boot_single_flight_gate_puro() {
        // Condição do gate: só trava com sessão pendente E boot em voo —
        // key-repeat/duplo Ctrl+N durante o "conectando…" não re-dispara.
        assert!(
            boot_single_flight_trava(false, true),
            "sessão pendente + boot em voo → trava"
        );
        assert!(
            !boot_single_flight_trava(false, false),
            "boot não está em voo (falhou antes) → retry liberado"
        );
        assert!(
            !boot_single_flight_trava(true, true),
            "sessão pronta → Ctrl+N é criação normal, sem gate"
        );
        assert!(!boot_single_flight_trava(true, false));
    }

    #[test]
    fn app_booting_destrava_nos_3_bracos_do_apply() {
        // TuiApp::new nasce sem boot em voo; as RESPOSTAS do boot (sucesso
        // via NewSession, falhas via Notice e RuntimeDead) destravam o gate
        // em apply_ui_update — nunca fica travado para sempre.
        let mut app = TuiApp::new("", "w", false);
        assert!(!app.booting && !app.session_ready, "default: sem boot em voo");
        let mut created = serde_json::json!({});
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel(String::new());

        // Falha (Notice) → destrava (braço 1 de falha; retry via Ctrl+N).
        app.booting = true;
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::Notice("falha ao criar sessão: x".into()),
        );
        assert!(!app.booting, "Notice destrava o single-flight");
        assert!(!app.session_ready, "falha não marca session_ready");

        // Sucesso (NewSession) → destrava + session_ready.
        app.booting = true;
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::NewSession {
                created: serde_json::json!({"sessionId": "s1"}),
                sid: "s1".into(),
                model: String::new(),
            },
        );
        assert!(!app.booting, "NewSession destrava o single-flight");
        assert!(app.session_ready, "boot completo marca session_ready");

        // RuntimeDead → destrava (braço 2 de falha).
        app.booting = true;
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::RuntimeDead { notice: "runtime morto".into() },
        );
        assert!(!app.booting, "RuntimeDead destrava o single-flight");
    }

    #[test]
    fn newsessao_limpa_busca_e_restaura_buffer() {
        let mut app = TuiApp::new("sess_old", "w", false);
        let mut created = serde_json::json!({});
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel("sess_old".to_string());
        app.push_msg("user", "pergunta antiga com alvo");
        app.input = "rascunho".into();
        app.cursor = app.input_len();
        // Busca aberta com matches da sessão antiga.
        app.open_search();
        app.search_insert_str("alvo");
        assert!(app.search.as_ref().is_some_and(|s| !s.matches.is_empty()));
        // Ctrl+N (NewSession): busca descartada, buffer restaurado.
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::NewSession {
                created: serde_json::json!({"session": {"sessionId": "sess_new"}}),
                sid: "sess_new".into(),
                model: String::new(),
            },
        );
        assert!(app.search.is_none(), "matches antigos não sobrevivem à troca");
        assert_eq!(app.input, "rascunho", "buffer do input restaurado intacto");
    }

    // ----- /export (Fase V4-2): caminho default + escrita real -----

    #[test]
    fn export_default_path_sid8_e_json() {
        assert_eq!(
            export_default_path("sess_abff123456789", false),
            "zcode-export-sess_abf.md"
        );
        assert_eq!(
            export_default_path("sess_abff123456789", true),
            "zcode-export-sess_abf.json"
        );
        // Sid curto usa o que tem; vazio degrada sem nome quebrado.
        assert_eq!(export_default_path("abc", false), "zcode-export-abc.md");
        assert_eq!(export_default_path("", true), "zcode-export-sess.json");
    }

    #[test]
    fn export_tuples_filtra_splash() {
        let mut app = TuiApp::new("sess_e", "w", false);
        app.push_msg("system", art::SPLASH_MARKER);
        app.push_msg("system", "notice");
        app.messages.push(ChatMsg {
            role: "assistant".into(),
            text: "pensando".into(),
            kind: crate::session::MsgKind::Reasoning,
            wire: None,
        });
        let t = export_tuples(&app);
        assert_eq!(t.len(), 2, "splash-marker filtrada");
        assert_eq!(t[0].0, "system");
        assert_eq!(t[1].2, crate::session::MsgKind::Reasoning);
    }

    #[test]
    fn export_write_md_e_json_roundtrip() {
        let dir = std::env::temp_dir().join(format!("zc-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let msgs = vec![
            ("user".to_string(), "pergunta com \"aspas\"".to_string(), crate::session::MsgKind::Text),
            ("assistant".to_string(), "pensamento".to_string(), crate::session::MsgKind::Reasoning),
        ];
        // Markdown no caminho dado: conteúdo com seções e thinking quote.
        let md_path = dir.join("conversa.md");
        let p = write_export(&msgs, "sess_abff123456789", "zai/glm-5.3-Flash", Some(md_path.to_str().unwrap()), false).unwrap();
        assert!(p.is_absolute(), "notice usa caminho absoluto");
        let md = std::fs::read_to_string(&p).unwrap();
        assert!(md.contains("# Conversa zcode-cli (sessão sess_abf,"));
        assert!(md.contains("## USER"));
        assert!(md.contains("> (thinking)"));
        // JSON default (sem caminho): escrito no CWD da biblioteca de teste —
        // usar caminho explícito p/ não sujar o cwd; aqui valida --json.
        let json_path = dir.join("conversa.json");
        let pj = write_export(&msgs, "sess_abff123456789", "m", Some(json_path.to_str().unwrap()), true).unwrap();
        let txt = std::fs::read_to_string(&pj).unwrap();
        let v: Value = serde_json::from_str(&txt).unwrap();
        assert_eq!(v["messages"][0]["text"], "pergunta com \"aspas\"");
        assert_eq!(v["messages"][1]["kind"], "reasoning");
        // Erro de escrita vira Err(String) claro (diretório como arquivo).
        let ruim = dir.join("nao_existe_dir"); // ainda não criado
        let e = write_export(&msgs, "s", "m", Some(ruim.join("x.md").to_str().unwrap()), false);
        assert!(e.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_config_toml_parseia() {
        let body = default_config_toml("");
        let cfg = config::parse_toml_str(&body).expect("config init deve parsear");
        assert_eq!(cfg.runtime.node, "node");
        assert_eq!(cfg.default.mode, "build");
        assert_eq!(cfg.ui.theme, "dark");
        assert_eq!(cfg.ui.stream_refresh_ms, 300);
        // Com runtime hint: caminho entra no TOML.
        let body2 = default_config_toml("C:/Users/x/zcode.cjs");
        let cfg2 = config::parse_toml_str(&body2).unwrap();
        assert!(cfg2.runtime.zcode_cjs.contains("zcode.cjs"));
    }

    #[test]
    fn collect_picker_rows_usa_parse_session_list() {
        let v = serde_json::json!({
            "sessions": [
                {"sessionId": "sess_a", "title": "t1", "status": "idle", "createdAt": "2026"},
                {"sessionId": "", "title": "sem id — descartada"},
                {"sessionId": "sess_b", "title": "t2", "status": "working", "createdAt": "2027"},
            ]
        });
        let rows = session::parse_session_list(&v);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "sess_a");
        assert_eq!(rows[1].title, "t2");
    }
}
