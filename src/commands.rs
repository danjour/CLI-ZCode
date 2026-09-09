//! Orquestração headless: dispatch de flags/subcomandos, REPL, shutdown gracioso.

use crate::cli::{Cli, Commands};
use crate::config;
use crate::doctor;
use crate::runtime::Runtime;
use crate::session::{self, HistoryEntry};
use crate::ui::tui::{self, ChatMsg, TuiApp, TuiKey};
use crate::ui::{art, input, render, theme};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use serde_json::Value;
use std::io::{BufRead, Write};
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

/// Aplica model/mode/thought SOMENTE quando o usuário passou a flag
/// explicitamente. Divergência verificada 2026-09-08: o servidor ao vivo
/// rejeita `zai/glm-5.3` ("Available models: main, zai/glm-5.3-Flash") e o
/// thought corrente é `low` — forçar defaults do config causaria erro/slowdown.
pub(crate) async fn apply_overrides(
    rt: &Arc<Runtime>,
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

/// Snapshot mínimo do workspace: {caminho-relativo → mtime-secs}.
/// Usado p/ diff pós-turno (arquivos criados/modificados).
pub fn snapshot_workspace(ws: &str) -> std::collections::HashMap<String, u64> {
    let mut out = std::collections::HashMap::new();
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
            let p = ent.path();
            if p.is_dir() {
                let name = ent.file_name().to_string_lossy().to_string();
                if name == ".git" || name == "node_modules" || name == "target" {
                    continue;
                }
                stack.push(p);
            } else if let Ok(md) = ent.metadata() {
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

/// Shutdown gracioso: session/close + kill da árvore.
pub(crate) async fn shutdown(rt: &Arc<Runtime>, session_id: Option<&str>) {
    if let Some(s) = session_id {
        rt.close_session_best_effort(s).await;
    }
    rt.kill_tree().await;
}

/// Ctrl+C durante o REPL: tenta `session/stop` (para o turn) e VOLTA ao
/// prompt limpo — não mata o CLI nem o runtime (requisito do handoff front).
/// O loop de stdin bloqueante continua; o sinal só interrompe o turn.
async fn install_ctrlc_hook(rt: Arc<Runtime>, session_id: String) {
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

pub async fn run(cli: Cli) -> Result<(), CmdError> {
    let cfg = config::load_config();

    match &cli.command {
        // doctor: 100% local, zero sessão/gasto (Fase 6).
        Some(Commands::Doctor) => doctor::run(&cli).await,
        Some(Commands::Sessions { limit }) => {
            let (rt, _) = spawn_runtime(&cli, &cfg).await?;
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
            rt.kill_tree().await;
            Ok(())
        }
        Some(Commands::New { pasta }) => {
            let ws = config::normalize_workspace(pasta);
            validate_workspace_exists(&ws)?;
            warn_tools_filter(&cli);
            let (rt, _) = spawn_runtime(&cli, &cfg).await?;
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
            let (rt, _) = spawn_runtime(&cli, &cfg).await?;
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
            rt.kill_tree().await;
            Ok(())
        }
        Some(Commands::Usage { id }) => {
            let (rt, _) = spawn_runtime(&cli, &cfg).await?;
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
            rt.kill_tree().await;
            Ok(())
        }
        Some(Commands::Resume { id }) => {
            let ws = resolve_workspace(&cli, &cfg);
            let target = match id.clone() {
                Some(s) => {
                    validate_resume_id(&s)?;
                    s
                }
                None if cli.cont => session::latest_for_workspace(&ws)
                    .map(|e| e.session_id)
                    .ok_or_else(|| {
                        CmdError::Session(format!(
                            "nenhuma sessão anterior em {ws} (use resume <id>)"
                        ))
                    })?,
                None => {
                    return Err(CmdError::Session(
                        "informe o id ou use -c p/ a última da pasta".into(),
                    ))
                }
            };
            let (rt, _) = spawn_runtime(&cli, &cfg).await?;
            let resumed: Value = rt
                .call("session/resume", session::resume_params(&target), 60)
                .await?;
            let sid = session::extract_session_id(&resumed).unwrap_or_else(|| target.clone());
            // R1: resume = LEITURA. Novo turno em retomada falha server-side
            // (-32031); por isso não enviamos aqui — mostramos leitura + aviso.
            show_resumed_reading(&rt, &sid).await;
            if let Some(p) = cli.prompt.clone() {
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
                    let (rt, _) = spawn_runtime(&cli, &cfg).await?;
                    let resumed: Value = rt
                        .call("session/resume", session::resume_params(&target), 60)
                        .await?;
                    let sid =
                        session::extract_session_id(&resumed).unwrap_or_else(|| target.clone());
                    show_resumed_reading(&rt, &sid).await;
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
                let (rt, _) = spawn_runtime(&cli, &cfg).await?;
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
                let (rt, _) = spawn_runtime(&cli, &cfg).await?;
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
    let (rt, _) = spawn_runtime(cli, cfg).await?;
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
    let (_raw, text) = session::send_and_wait(&rt, &sid, prompt, 300)
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
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    };
    use ratatui::{backend::CrosstermBackend, Terminal};

    let cfg = config::load_config();
    let ws = resolve_workspace(&cli, &cfg);
    validate_workspace_exists(&ws)?;
    let (rt, _) = spawn_runtime(&cli, &cfg).await?;
    let mut created = session::create_session(&rt, &ws).await?;
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
        model: eff.clone(),
        updated_at: now_rfc3339(),
    });
    // Push stream (best-effort): as notificações agora são ENTREGUES ao loop
    // (canal do runtime + gatilho de fetch no poller); o poll por intervalo
    // continua como fallback se nenhuma chegar.
    if let Err(e) = rt
        .call("session/subscribe", tui_subscribe_params(&sid), 15)
        .await
    {
        tracing::warn!(%e, "subscribe falhou (poll mantido)");
    }

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

    let mut app = TuiApp::new(&sid, &ws, false);
    app.model = eff;
    app.mode = cli.mode.clone().unwrap_or_else(|| cfg.default.mode.clone());
    app.ctx = session::parse_context(&created);
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
    // sessão ativa p/ o poller (Ctrl+N). poll_wake: notificação → fetch
    // imediato, sem esperar o intervalo.
    let (ui_tx, mut ui_rx): (UiTx, UiRx) = tokio::sync::mpsc::unbounded_channel();
    let (working_tx, working_rx) = tokio::sync::watch::channel(false);
    let (sid_tx, sid_rx) = tokio::sync::watch::channel(sid.clone());
    let poll_wake = std::sync::Arc::new(tokio::sync::Notify::new());

    // Poller dedicado (A): rede fora do braço de teclado. Termina quando a
    // task da UI sai (o receptor do canal cai → send falha → break).
    {
        let rt2 = rt.clone();
        let ui_tx2 = ui_tx.clone();
        let mut working_rx2 = working_rx.clone();
        let mut sid_rx2 = sid_rx.clone();
        let wake2 = poll_wake.clone();
        let cfg_ms = cfg.ui.stream_refresh_ms;
        tokio::spawn(async move {
            let mut poll_sid = sid_rx2.borrow_and_update().clone();
            let mut snapshot: Vec<session::MsgItem> = Vec::new();
            let mut fail_streak: u32 = 0;
            let mut dead_notified = false;
            loop {
                let working = *working_rx2.borrow_and_update();
                let ms = poll_interval_ms(working, cfg_ms);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(ms)) => {}
                    _ = wake2.notified() => {} // notificação → fetch imediato
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
                let fetched = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    session::fetch_messages(&rt2, &poll_sid),
                )
                .await;
                match fetched {
                    Ok(Ok(msgs)) => {
                        fail_streak = 0;
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
                        // (zera, igual ao timeout do runtime).
                        fail_streak = 0;
                    }
                }
            }
        });
    }

    // Ponte de notificações do runtime (E): qualquer push do filho desperta o
    // poller (fetch imediato) e marca a UI dirty. O shape dos eventos NÃO é
    // confirmado no protocolo — nada é interpretado além do nome do método;
    // sem notificações, o poll por intervalo segue funcionando (fallback).
    if let Some(mut ev_rx) = rt.take_event_rx().await {
        let ui_tx3 = ui_tx.clone();
        let wake3 = poll_wake.clone();
        tokio::spawn(async move {
            while let Some(msg) = ev_rx.recv().await {
                wake3.notify_one();
                let method = msg
                    .get("method")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string();
                if ui_tx3.send(UiUpdate::SessionEvent { method }).is_err() {
                    break;
                }
            }
        });
    }

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
                if let Ok(out) = h.await {
                    app.working = false;
                    // Sem `poll_wake.notify_one()` aqui de propósito: o fim de
                    // turno não precisa de fetch imediato — a cadência idle
                    // (1,5s) é aceitável e evita um poll extra por turno.
                    let _ = working_tx.send(false);
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
                            app.last_stats = Some(st);
                            // Ctx% ao vivo: projection do `session/send`
                            // (parse_context já degrada p/ zeros — silencioso).
                            let c = session::parse_context(&proj);
                            if c.window > 0 {
                                app.ctx = c;
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
                    rt: &rt,
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
    shutdown(&rt, Some(&app.session_id)).await;
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
    /// Ctrl+N: sessão criada (subscribe já feito na task).
    NewSession { created: Value, sid: String },
    /// Notificação push do runtime (shape não confirmado — só o método é
    /// propagado; serve para marcar a UI dirty).
    SessionEvent { method: String },
    /// Runtime morto detectado (task do turno ou poller): banner único +
    /// `app.runtime_dead = true` (trava novos turnos; header vira DEAD).
    RuntimeDead { notice: String },
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

pub fn poll_interval_ms(working: bool, configured_ms: u64) -> u64 {
    if working {
        configured_ms.max(100)
    } else {
        IDLE_POLL_MS
    }
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
        UiUpdate::Notice(s) => app.push_msg("system", &s),
        UiUpdate::NewSession { created: c, sid } => {
            *created = c;
            app.session_id = sid.clone();
            app.messages.clear();
            app.bump_rev();
            app.scroll = 0;
            app.follow = true;
            app.ctx = session::parse_context(created);
            app.last_stats = None;
            // Sessão nova só é possível com runtime vivo: limpa a trava.
            app.runtime_dead = false;
            app.push_msg("system", &format!("sessão nova: {sid}"));
            // Poller troca de sessão + reseta snapshot. Sem notify_one aqui:
            // o braço `sid_rx.changed()` do select do poller já interrompe o
            // sleep → refetch imediato (watch guarda a mudança pendente).
            let _ = sid_tx.send(sid);
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
            app.push_msg("system", &notice);
        }
    }
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
/// UI; RPCs saem por `tokio::spawn` e devolvem pelo canal (B).
struct KeyCtx<'a> {
    app: &'a mut TuiApp,
    created: &'a mut Value,
    rt: &'a Arc<Runtime>,
    ws: &'a str,
    send_task: &'a mut Option<TurnJoin>,
    working_tx: &'a tokio::sync::watch::Sender<bool>,
    ui_tx: &'a UiTx,
    /// Gatilho do fetch imediato do poller (Enter → working).
    poll_wake: &'a std::sync::Arc<tokio::sync::Notify>,
    /// Diff do último turno concluído (p/ o /diff; vive no escopo do loop).
    last_diff: &'a mut Option<WorkspaceDiff>,
}

/// Handler de teclado/mouse: só mutação local otimista + spawn de RPC. Nenhuma
/// await de rede aqui — a tecla responde no mesmo frame. Retorna `true` se o
/// evento foi consumido com efeito visível (→ dirty); o mouse só "conta" em
/// scroll da roda (Moved/Drag/cliques não provocam redraw).
fn handle_key_event(ctx: &mut KeyCtx, ev: Event) -> bool {
    use crossterm::event::MouseEventKind;

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
                let rt2 = ctx.rt.clone();
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
                let rt2 = ctx.rt.clone();
                let sid2 = ctx.app.session_id.clone();
                let tx2 = ctx.ui_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = session::stop_turn(&rt2, &sid2).await {
                        let _ = tx2.send(UiUpdate::Notice(format!("stop falhou: {e}")));
                    }
                });
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
            ctx.app.push_msg("system", "(criando sessão…)");
            let rt2 = ctx.rt.clone();
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
                        let _ = tx2.send(UiUpdate::NewSession { created: c, sid });
                    }
                    Err(e) => {
                        let _ = tx2.send(UiUpdate::Notice(format!("new falhou: {e}")));
                    }
                }
            });
        }
        TuiKey::CtrlU => {
            let rt2 = ctx.rt.clone();
            let sid2 = ctx.app.session_id.clone();
            let tx2 = ctx.ui_tx.clone();
            tokio::spawn(async move {
                let upd = match session::fetch_usage(&rt2, &sid2).await {
                    Ok(u) => UiUpdate::Usage {
                        usage: u,
                        notice: false,
                    },
                    Err(e) => UiUpdate::Notice(format!("usage falhou: {e}")),
                };
                let _ = tx2.send(upd);
            });
        }
        TuiKey::CtrlP => {
            // Tempo 2: sem RPC de approve/deny — componente oculto + nota.
            ctx.app.push_msg("system", &render::perm_unsupported_note());
        }
        TuiKey::Enter => {
            ctx.app.disarm_ctrlc();
            let buf = std::mem::take(&mut ctx.app.input);
            ctx.app.cursor = 0;
            if buf.trim().is_empty() {
                return true;
            }
            // Slash de uma linha → comandos Fase 3; resto → turno.
            let slash = !buf.contains('\n') && buf.trim_start().starts_with('/');
            if slash {
                handle_tui_slash(
                    ctx.rt,
                    ctx.app,
                    ctx.created,
                    buf.trim(),
                    ctx.ui_tx,
                    ctx.ws,
                    ctx.last_diff,
                );
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
                ctx.app.push_msg("system", "aguarde o turn (Esc cancela).");
                ctx.app.input = buf;
                ctx.app.cursor = ctx.app.input_len();
            } else {
                // Estado otimista IMEDIATO (B): mensagem + working no mesmo
                // frame; usage-before, send, usage-after, snapshot e git stat
                // ficam na task.
                ctx.app.push_msg("user", buf.trim());
                // Histórico de prompts p/ recall ↑/↓ — só no envio REAL
                // (slash/read_only/runtime_dead/working não registram).
                ctx.app.remember_prompt(buf.trim());
                ctx.app.working = true;
                ctx.app.status = "working…".to_string();
                let rt2 = ctx.rt.clone();
                let sid2 = ctx.app.session_id.clone();
                let content = buf.trim().to_string();
                let ws2 = ctx.ws.to_string();
                *ctx.send_task = Some(tokio::spawn(async move {
                    let start = std::time::Instant::now();
                    // Snapshot ANTES do send (diff pós-turno); o custo fica na
                    // task, fora do caminho do draw (snapshot limitado a 20k).
                    let snap_before = snapshot_workspace(&ws2);
                    let before = session::fetch_usage(&rt2, &sid2).await.unwrap_or_default();
                    // Triple: (poll bruto, texto, projection do session/send).
                    let res = session::send_and_wait_fast(&rt2, &sid2, &content, 300).await;
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
                let _ = ctx.working_tx.send(true); // poller acelera
                // Acorda o poller AGORA: sem isso, o 1º fetch do turno só
                // aconteceria no fim do intervalo atual (working ou idle).
                ctx.poll_wake.notify_one();
            }
        }
    }
    true // tecla consumida (inclusive Ignore) — mantém o redraw imediato
}

/// Mensagem local bate com o item do servidor (role, texto E kind — reasoning
/// novo/divergente tem que atualizar, não ser ignorado)?
fn msg_eq(m: &ChatMsg, s: &session::MsgItem) -> bool {
    m.role == s.role && m.text == s.text && m.kind == s.kind
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
    rt: &Arc<Runtime>,
    app: &mut TuiApp,
    created: &Value,
    line: &str,
    tx: &UiTx,
    ws: &str,
    last_diff: &Option<WorkspaceDiff>,
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
            let rt2 = rt.clone();
            let sid = app.session_id.clone();
            let tx2 = tx.clone();
            tokio::spawn(async move {
                let upd = match session::fetch_usage(&rt2, &sid).await {
                    Ok(u) => UiUpdate::Usage {
                        usage: u,
                        notice: true,
                    },
                    Err(e) => UiUpdate::Notice(format!("usage falhou: {e}")),
                };
                let _ = tx2.send(upd);
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
        input::Input::Resume(_) => app.push_msg("system", &render::resume_readonly_notice()),
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
async fn show_resumed_reading(rt: &Arc<Runtime>, session_id: &str) {
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
    rt: &Arc<Runtime>,
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
            input::Input::Resume(_) => {
                // R1: retomada = leitura; sem RPC novo aqui (zero gasto).
                println!("{}", render::resume_readonly_notice());
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
            input::Input::Text(t) => match session::send_and_wait(rt, session_id, &t, 300).await {
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
            },
        }
    }
    Ok(())
}

/// REPL de leitura (R1): não envia turnos; só /usage, /goal show e /exit
/// executam — o resto recebe o aviso R1 (zero gasto).
async fn repl_loop_readonly(
    rt: &Arc<Runtime>,
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
            input::Input::Usage => match session::fetch_usage(rt, session_id).await {
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
                match session::goal_action(rt, session_id, &a).await {
                    Ok(res) => println!(
                        "{}",
                        serde_json::to_string_pretty(&res).unwrap_or_else(|_| "{}".into())
                    ),
                    Err(e) => eprintln!("goal falhou: {e}"),
                }
            }
            input::Input::Text(_) => {
                if cli.json {
                    println!(
                        "{}",
                        serde_json::json!({ "sessionId": session_id, "readOnly": true })
                    );
                } else {
                    println!("{}", render::resume_readonly_notice());
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
        let snap = snapshot_workspace(&dir.to_string_lossy());
        assert_eq!(snap.len(), 1);
        assert!(snap.contains_key("f.txt"));
        // Não é repo git (ou git ausente) → None, sem erro.
        assert!(git_diff_stat(&dir.to_string_lossy()).is_none());
        let _ = std::fs::remove_dir_all(&dir);
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
            "projection": {"contextWindow": 10, "contextUsed": 5}
        });
        app.runtime_dead = true; // trava de sessão morta é limpa pela sessão nova
        apply_ui_update(
            &mut app,
            &mut created,
            &sid_tx,
            UiUpdate::NewSession {
                created: created_new,
                sid: "sess_n".into(),
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

    // ----- paste por timing: Enter colado vira newline (Windows nativo) -----

    use crossterm::event::{KeyEvent, KeyModifiers};

    fn tecla(code: KeyCode, mods: KeyModifiers, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent::new_with_kind(code, mods, kind))
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
}
