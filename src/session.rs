//! Sessões: builders de params (testáveis), parsing de results, ops async e
//! persistência local JSON (escolha documentada em `config.rs`).
//!
//! Persistência: `~/.local/share/zcode-cli/history.json`
//! `[{sessionId, title, workspace, model, updatedAt}]`.

use crate::config;
// Fase 3 (daemon): a camada do meio fala com o TRANSPORTE unificado —
// `Transport::call` reproduz a assinatura de `Runtime::call`, então os
// corpos abaixo não mudaram de shape (Embedded ou Daemon, indiferente).
use crate::daemon_client::Transport;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("runtime: {0}")]
    Runtime(#[from] crate::runtime::RuntimeError),
    #[error("config: {0}")]
    Config(String),
    #[error("protocolo: {0}")]
    Protocol(String),
    #[error("io: {0}")]
    Io(String),
}

// ---------- builders puros (shapes plano §3.1–3.3) ----------

pub fn create_params(workspace: &str) -> Value {
    serde_json::json!({ "workspace": { "workspacePath": workspace, "workspaceKey": workspace } })
}

/// session/send usa `content` (NÃO `message`).
pub fn send_params(session_id: &str, content: &str) -> Value {
    serde_json::json!({ "sessionId": session_id, "content": content })
}

pub fn messages_params(session_id: &str) -> Value {
    serde_json::json!({ "sessionId": session_id })
}

pub fn list_params(limit: u32) -> Value {
    serde_json::json!({ "limit": limit })
}

pub fn usage_params(session_id: &str) -> Value {
    serde_json::json!({ "sessionId": session_id })
}

/// setModel exige OBJETO `{providerId, modelId}` — nunca string.
pub fn set_model_params(session_id: &str, provider_id: &str, model_id: &str) -> Value {
    serde_json::json!({
        "sessionId": session_id,
        "model": { "providerId": provider_id, "modelId": model_id },
    })
}

pub fn set_mode_params(session_id: &str, mode: &str) -> Value {
    serde_json::json!({ "sessionId": session_id, "mode": mode })
}

pub fn set_thought_params(session_id: &str, level: &str) -> Value {
    serde_json::json!({ "sessionId": session_id, "thoughtLevel": level })
}

pub fn resume_params(session_id: &str) -> Value {
    serde_json::json!({ "sessionId": session_id })
}

pub fn stop_params(session_id: &str) -> Value {
    serde_json::json!({ "sessionId": session_id })
}

/// Roadmap: graceful shutdown via `session/close` (docs/PLANO-CLI-ZCODE.md,
/// "Graceful shutdown") — builder pronto p/ a fase de encerramento limpo.
#[allow(dead_code)]
pub fn close_params(session_id: &str) -> Value {
    serde_json::json!({ "sessionId": session_id })
}

pub fn fork_params(session_id: &str) -> Value {
    serde_json::json!({ "sessionId": session_id })
}

/// session/compact — VERIFICADO ao vivo 2026-09-08 (Fase 3): método existe;
/// `{}` → -32602 exigindo só `sessionId` (ZodError de issue única).
/// Mantido mínimo: só sessionId (sem inventar `instructions` etc.).
pub fn compact_params(session_id: &str) -> Value {
    serde_json::json!({ "sessionId": session_id })
}

/// Ações de session/goal — VERIFICADAS ao vivo 2026-09-08 (Fase 3):
/// enum do servidor = show|set|replace|pause|resume|clear.
/// `{sessionId, action:"show"}` passa na validação (-32004 só por sessão fake).
/// NOTA: set/replace provavelmente exigem campo de texto cujo nome NÃO foi
/// verificado — NÃO implementados (sem invenção; 1 sonda futura resolve).
pub fn goal_params(session_id: &str, action: &str) -> Value {
    serde_json::json!({ "sessionId": session_id, "action": action })
}

pub fn validate_goal_action(action: &str) -> Result<(), SessionError> {
    match action {
        "show" | "set" | "replace" | "pause" | "resume" | "clear" => Ok(()),
        a => Err(SessionError::Protocol(format!(
            "goal action inválida '{a}': use show|set|replace|pause|resume|clear"
        ))),
    }
}

pub fn validate_mode(mode: &str) -> Result<(), SessionError> {
    match mode {
        "plan" | "build" | "edit" | "yolo" => Ok(()),
        m => Err(SessionError::Protocol(format!("mode inválido '{m}': use plan|build|edit|yolo"))),
    }
}

pub fn validate_thought(level: &str) -> Result<(), SessionError> {
    match level {
        "low" | "high" | "max" => Ok(()),
        l => Err(SessionError::Protocol(format!("thought-level inválido '{l}': use low|high|max"))),
    }
}

// ---------- parsing de results ----------

pub fn extract_session_id(create_result: &Value) -> Option<String> {
    create_result
        .get("session")?.get("sessionId")?.as_str()
        .map(|s| s.to_string())
}

pub fn extract_protocol_version(create_result: &Value) -> Option<u64> {
    create_result.get("protocol")?.get("version")?.as_u64()
}

/// Modelo efetivo do servidor (`settings.model.current` → "provider/model").
/// Cai para `lastUsed`, depois para o default do plano.
pub fn effective_model(create_or_resume_result: &Value) -> String {
    let cur = create_or_resume_result
        .get("settings").and_then(|s| s.get("model")).and_then(|m| m.get("current"))
        .or_else(|| create_or_resume_result
            .get("settings").and_then(|s| s.get("model")).and_then(|m| m.get("lastUsed")));
    match (cur.and_then(|c| c.get("providerId")).and_then(|p| p.as_str()),
           cur.and_then(|c| c.get("modelId")).and_then(|m| m.as_str())) {
        (Some(p), Some(m)) => format!("{p}/{m}"),
        _ => config::default_model(),
    }
}

/// Modelo da lista settings.model.available (para o /model da Maria).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AvailableModel {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub provider_id: String,
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub context_window: u64,
}

/// Extrai `settings.model.available[]` do create (sem nova chamada).
/// Shape real: [{label, ref:{providerId,modelId}, contextWindow, ...}].
pub fn available_models(create_or_resume_result: &Value) -> Vec<AvailableModel> {
    create_or_resume_result
        .get("settings").and_then(|s| s.get("model")).and_then(|m| m.get("available"))
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|m| AvailableModel {
            label: m.get("label").and_then(|l| l.as_str()).unwrap_or("").to_string(),
            provider_id: m.get("ref").and_then(|r| r.get("providerId")).and_then(|p| p.as_str()).unwrap_or("").to_string(),
            model_id: m.get("ref").and_then(|r| r.get("modelId")).and_then(|p| p.as_str()).unwrap_or("").to_string(),
            context_window: m.get("contextWindow").and_then(|c| c.as_u64()).unwrap_or(0),
        })
        .filter(|m| !m.model_id.is_empty())
        .collect()
}

/// Tipo de item de transcript: texto normal ou raciocínio (reasoning).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MsgKind {
    #[default]
    Text,
    Reasoning,
}

/// Item de transcript p/ exibição: role + texto + tipo. Mensagens assistant
/// com `parts` de `type=="reasoning"` ganham item PRÓPRIO, emitido ANTES do
/// item de texto (ordem real das parts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsgItem {
    pub role: String,
    pub text: String,
    pub kind: MsgKind,
}

fn message_role(m: &Value) -> String {
    m.get("info").and_then(|i| i.get("role")).and_then(|r| r.as_str())
        .or_else(|| m.get("role").and_then(|r| r.as_str()))
        .unwrap_or("?")
        .to_string()
}

/// Itens de exibição de `session/messages` (texto + reasoning).
/// SHAPE REAL (verificado 2026-09-08): role fica em `info.role` (não em
/// `role`); `parts[]` mistura `step-start`/`reasoning`/`text`/`step-finish`.
/// Para cada mensagem com `reasoning` não-vazio, emite `(role, texto,
/// Reasoning)` ANTES do item de texto; mensagens sem texto nem reasoning não
/// geram item (mesmo filtro de vazio do wrapper abaixo).
pub fn extract_display_items(messages_result: &Value) -> Vec<MsgItem> {
    let arr = messages_result
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .or_else(|| messages_result.as_array().cloned())
        .unwrap_or_default();
    let mut out = Vec::new();
    for m in &arr {
        let role = message_role(m);
        let mut reasoning = String::new();
        let mut text = String::new();
        if let Some(parts) = m.get("parts").and_then(|p| p.as_array()) {
            for pt in parts {
                match pt.get("type").and_then(|t| t.as_str()) {
                    Some("reasoning") => {
                        if let Some(t) = pt.get("text").and_then(|t| t.as_str()) {
                            reasoning.push_str(t);
                        }
                    }
                    Some("text") => {
                        if let Some(t) = pt.get("text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                        }
                    }
                    _ => {}
                }
            }
        }
        if !reasoning.trim().is_empty() {
            out.push(MsgItem { role: role.clone(), text: reasoning, kind: MsgKind::Reasoning });
        }
        if !text.trim().is_empty() {
            out.push(MsgItem { role, text, kind: MsgKind::Text });
        }
    }
    out
}

/// `(role, text)` de cada mensagem de TEXTO de `session/messages` — wrapper
/// de `extract_display_items` que filtra `kind == Text`. REPL/headless
/// continuam aqui sem mudança de comportamento (reasoning não entra no texto
/// do turno nem no `last_assistant_text`).
pub fn extract_text_messages(messages_result: &Value) -> Vec<(String, String)> {
    extract_display_items(messages_result)
        .into_iter()
        .filter(|i| i.kind == MsgKind::Text)
        .map(|i| (i.role, i.text))
        .collect()
}

pub fn last_assistant_text(messages_result: &Value) -> Option<String> {
    extract_text_messages(messages_result)
        .into_iter()
        .rev()
        .find(|(role, _)| role == "assistant")
        .map(|(_, t)| t)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub model_request_count: u64,
}

pub fn parse_usage(v: &Value) -> Usage {
    let g = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    // Servidor usa camelCase; aceita ambos.
    let pick = |camel: &str, snake: &str| {
        v.get(camel).and_then(|x| x.as_u64()).or_else(|| v.get(snake).and_then(|x| x.as_u64())).unwrap_or(0)
    };
    Usage {
        total_tokens: pick("totalTokens", "total_tokens").max(g("totalTokens")),
        input_tokens: pick("inputTokens", "input_tokens"),
        output_tokens: pick("outputTokens", "output_tokens"),
        reasoning_tokens: pick("reasoningTokens", "reasoning_tokens"),
        cache_creation_tokens: pick("cacheCreationTokens", "cache_creation_tokens"),
        cache_read_tokens: pick("cacheReadTokens", "cache_read_tokens"),
        model_request_count: pick("modelRequestCount", "model_request_count"),
    }
}

/// Métricas de um turno para a statusbar rica da TUI (adendo TUI-visual).
/// `tok_per_s` é calculado sobre `output_tokens` do delta.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnStats {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub elapsed_ms: u64,
    pub tok_per_s: f64,
}

/// Deltas (saturantes) entre `session/usage` antes/depois do turno.
/// Puro e fixture-testável; a TUI mede `elapsed` com `Instant`.
pub fn measure_turn(before: &Usage, after: &Usage, elapsed: std::time::Duration) -> TurnStats {
    let st = TurnStats {
        input_tokens: after.input_tokens.saturating_sub(before.input_tokens),
        output_tokens: after.output_tokens.saturating_sub(before.output_tokens),
        total_tokens: after.total_tokens.saturating_sub(before.total_tokens),
        elapsed_ms: elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
        tok_per_s: 0.0,
    };
    let secs = elapsed.as_secs_f64();
    TurnStats {
        tok_per_s: if secs > 0.0 { st.output_tokens as f64 / secs } else { 0.0 },
        ..st
    }
}

// ---------- todos do agente (plano §3.4: result.todos/todoGroups) ----------

/// Status de um item do checklist do agente.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// Item do checklist do agente (`/todos`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

/// Mapeia o status STRING do servidor (defensivo): aceita as grafias
/// conhecidas ("pending"/"in-progress"/"in_progress"/"completed"/"done");
/// "cancelled" e desconhecidos caem em Pending — o enum só tem 3 estados e
/// "não concluído" é a leitura honesta (nada inventado).
fn todo_status(raw: &str) -> TodoStatus {
    match raw.trim().to_ascii_lowercase().as_str() {
        "completed" | "done" => TodoStatus::Completed,
        "in-progress" | "in_progress" | "inprogress" => TodoStatus::InProgress,
        _ => TodoStatus::Pending,
    }
}

/// Conteúdo textual de um item: primeiro não-vazio entre content/text/label.
fn todo_content(v: &Value) -> Option<&str> {
    ["content", "text", "label"].iter().find_map(|k| {
        v.get(*k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    })
}

/// Um item de qualquer shape tolerante: string pura (vira Pending) ou objeto
/// com content/text/label + status/state. Sem conteúdo legível → None.
fn todo_from_value(v: &Value) -> Option<TodoItem> {
    if let Some(s) = v.as_str() {
        let t = s.trim();
        return (!t.is_empty())
            .then(|| TodoItem { content: t.to_string(), status: TodoStatus::Pending });
    }
    let content = todo_content(v)?.to_string();
    let status = v
        .get("status")
        .or_else(|| v.get("state"))
        .and_then(|s| s.as_str())
        .map(todo_status)
        .unwrap_or(TodoStatus::Pending);
    Some(TodoItem { content, status })
}

/// Extrai o checklist de um result de `session/create`/`session/send` (plano
/// §3.4: `todos` ou `todoGroups`). 100% defensivo: raiz OU `projection`;
/// array de strings OU objetos (content/text/label + status); `todoGroups` é
/// achatado na melhor interpretação tolerante (lista interna
/// `todos`/`items`/`tasks`, string ou objeto direto). Shapes irreconhecíveis
/// ou ausentes → vazio — nunca panica, nunca inventa campo.
pub fn extract_todos(create_or_send_result: &Value) -> Vec<TodoItem> {
    // Placeholder p/ quando não há `projection` no result (static: sem
    // temporário solto no borrow).
    static NULL: Value = Value::Null;
    let fontes = [
        create_or_send_result,
        create_or_send_result.get("projection").unwrap_or(&NULL),
    ];
    // 1) `todos` direto (raiz primeiro, projection depois).
    for f in fontes {
        if let Some(arr) = f.get("todos").and_then(|t| t.as_array()) {
            let itens: Vec<TodoItem> = arr.iter().filter_map(todo_from_value).collect();
            if !itens.is_empty() {
                return itens;
            }
        }
    }
    // 2) `todoGroups` achatado (mesma tolerância, ambas as fontes).
    for f in fontes {
        if let Some(groups) = f.get("todoGroups").and_then(|g| g.as_array()) {
            let mut out: Vec<TodoItem> = Vec::new();
            for g in groups {
                let inner = ["todos", "items", "tasks"]
                    .iter()
                    .find_map(|k| g.get(*k).and_then(|x| x.as_array()));
                match inner {
                    Some(itens) => out.extend(itens.iter().filter_map(todo_from_value)),
                    // Grupo sem lista interna: string ou objeto com conteúdo
                    // próprio vira item direto (leitura tolerante).
                    None => {
                        if let Some(item) = todo_from_value(g) {
                            out.push(item);
                        }
                    }
                }
            }
            if !out.is_empty() {
                return out;
            }
        }
    }
    Vec::new()
}

/// Contexto do payload do create (plano §3.4: `projection` traz
/// `contextWindow`/`contextUsed`). Só parseia o que já vem — sem nova chamada.
/// Aceita o result inteiro do create ou o objeto `projection` direto;
/// ausente → zeros (sem quebrar nada existente).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextUsage {
    pub window: u64,
    pub used: u64,
}

impl ContextUsage {
    /// 0.0–100.0 (0.0 se window == 0).
    pub fn pct(&self) -> f64 {
        if self.window == 0 {
            0.0
        } else {
            (self.used as f64 / self.window as f64 * 100.0).min(100.0)
        }
    }
}

pub fn parse_context(create_or_projection: &Value) -> ContextUsage {
    let proj = create_or_projection.get("projection").unwrap_or(create_or_projection);
    ContextUsage {
        window: proj.get("contextWindow").and_then(|c| c.as_u64()).unwrap_or(0),
        used: proj.get("contextUsed").and_then(|c| c.as_u64()).unwrap_or(0),
    }
}

/// Atividade de tools do turno a partir do result do `session/send`
/// (DEFENSIVO: o shape de `activeToolCalls` NÃO foi verificado ao vivo).
/// Lê `projection.activeToolCalls` (ou o campo na raiz) só via `.get()`
/// encadeado; SOMENTE array não-vazio de objetos com `name`/`tool` string
/// não-vazia produz o resumo agregado — qualquer outro shape (ou um único
/// elemento inválido) → `None`, nada é emitido. Nunca panica, nunca inventa
/// campo obrigatório.
pub fn summarize_active_tools(send_result: &Value) -> Option<String> {
    let proj = send_result.get("projection").unwrap_or(send_result);
    let calls = proj
        .get("activeToolCalls")
        .or_else(|| send_result.get("activeToolCalls"))
        .and_then(|a| a.as_array())?;
    if calls.is_empty() {
        return None;
    }
    // Agregado por nome, preservando 1ª aparição (determinístico, sem map).
    let mut agg: Vec<(String, usize)> = Vec::new();
    for c in calls {
        let name = c
            .get("name")
            .and_then(|n| n.as_str())
            .or_else(|| c.get("tool").and_then(|t| t.as_str()))?;
        if name.trim().is_empty() {
            return None;
        }
        match agg.iter_mut().find(|(n, _)| n == name) {
            Some((_, k)) => *k += 1,
            None => agg.push((name.to_string(), 1)),
        }
    }
    if agg.is_empty() {
        return None;
    }
    let list = agg
        .iter()
        .map(|(n, k)| format!("{n} ×{k}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("tools neste turno: {list}"))
}

// ---------- export da conversa (/export, Fase V4-2) ----------

/// Sessão curta p/ o export (`zcode-export-{sid8}.md`): 8 primeiros chars.
pub fn sid_short8(sid: &str) -> String {
    sid.chars().take(8).collect()
}

/// Badge do role p/ o export (mesma semântica do transcript da TUI, local p/
/// não pendurar o núcleo na camada de UI): reasoning → THINK; senão o badge
/// clássico USER/ASSIST/SYS.
fn export_role_badge(role: &str, kind: MsgKind) -> &'static str {
    if kind == MsgKind::Reasoning {
        "THINK"
    } else {
        match role {
            "user" => "USER",
            "assistant" => "ASSIST",
            _ => "SYS",
        }
    }
}

/// Export Markdown (puro, testável): cabeçalho `# Conversa zcode-cli (sessão
/// {sid8}, modelo {model}, {data RFC3339})` + uma seção `## {badge}` por
/// mensagem com o texto cru; reasoning vira bloco quote `> (thinking)`.
/// O marcador de splash é filtrado pelo chamador (é detalhe da TUI).
pub fn build_export_md(
    messages: &[(String, String, MsgKind)],
    sid: &str,
    model: &str,
    quando: &str,
) -> String {
    let modelo = if model.is_empty() { "-" } else { model };
    let mut out = format!(
        "# Conversa zcode-cli (sessão {}, modelo {}, {})\n\n",
        sid_short8(sid),
        modelo,
        quando
    );
    if messages.is_empty() {
        out.push_str("(sem mensagens)\n");
        return out;
    }
    for (role, text, kind) in messages {
        out.push_str(&format!("## {}\n\n", export_role_badge(role, *kind)));
        if *kind == MsgKind::Reasoning {
            out.push_str("> (thinking)\n");
            for linha in text.lines() {
                out.push_str("> ");
                out.push_str(linha);
                out.push('\n');
            }
        } else {
            out.push_str(text);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// Export JSON (puro, testável): metadados da sessão + array de mensagens
/// `{role, kind: "text"|"reasoning", text}` (escaping fica por conta do
/// serde_json — texto cru sobrevive intacto).
pub fn build_export_json(
    messages: &[(String, String, MsgKind)],
    sid: &str,
    model: &str,
    quando: &str,
) -> Value {
    serde_json::json!({
        "sessionId": sid,
        "model": model,
        "exportedAt": quando,
        "messages": messages
            .iter()
            .map(|(role, text, kind)| serde_json::json!({
                "role": role,
                "kind": if *kind == MsgKind::Reasoning { "reasoning" } else { "text" },
                "text": text,
            }))
            .collect::<Vec<_>>(),
    })
}

// ---------- ops async ----------

pub async fn create_session(rt: &Arc<Transport>, workspace: &str) -> Result<Value, SessionError> {
    let res = rt.call("session/create", create_params(workspace), 60).await?;
    if let Some(v) = extract_protocol_version(&res) {
        if config::is_new_protocol_version(Some(v)) {
            tracing::warn!(version = v, "protocol.version > 1 — chamar orquestrador (shape pode ter mudado)");
        }
    }
    Ok(res)
}

pub async fn send_message(rt: &Arc<Transport>, session_id: &str, content: &str) -> Result<Value, SessionError> {
    Ok(rt.call("session/send", send_params(session_id, content), 120).await?)
}

pub async fn fetch_messages(rt: &Arc<Transport>, session_id: &str) -> Result<Value, SessionError> {
    Ok(rt.call("session/messages", messages_params(session_id), 30).await?)
}

pub async fn list_sessions(rt: &Arc<Transport>, limit: u32) -> Result<Value, SessionError> {
    Ok(rt.call("session/list", list_params(limit), 30).await?)
}

pub async fn fetch_usage(rt: &Arc<Transport>, session_id: &str) -> Result<Usage, SessionError> {
    let v = rt.call("session/usage", usage_params(session_id), 30).await?;
    Ok(parse_usage(&v))
}

pub async fn apply_model(rt: &Arc<Transport>, session_id: &str, model: &str) -> Result<Value, SessionError> {
    let (p, m) = config::split_model_ref(model).map_err(|e| SessionError::Config(e.to_string()))?;
    Ok(rt.call("session/setModel", set_model_params(session_id, &p, &m), 30).await?)
}

pub async fn apply_mode(rt: &Arc<Transport>, session_id: &str, mode: &str) -> Result<Value, SessionError> {
    validate_mode(mode)?;
    Ok(rt.call("session/setMode", set_mode_params(session_id, mode), 30).await?)
}

pub async fn apply_thought(rt: &Arc<Transport>, session_id: &str, level: &str) -> Result<Value, SessionError> {
    validate_thought(level)?;
    Ok(rt.call("session/setThoughtLevel", set_thought_params(session_id, level), 30).await?)
}

/// session/stop mid-turn (Fase 3: Ctrl+C durante turno → stop + volta ao prompt).
/// Builder já existia; op exposta para o wiring da Maria (sem UX aqui).
pub async fn stop_turn(rt: &Arc<Transport>, session_id: &str) -> Result<Value, SessionError> {
    Ok(rt.call("session/stop", stop_params(session_id), 15).await?)
}

/// session/compact (Fase 3). CUIDADO: em sessão real dispara sumarização via LLM
/// (gasta plano) — a Maria deve confirmar antes no /compact.
pub async fn compact_session(rt: &Arc<Transport>, session_id: &str) -> Result<Value, SessionError> {
    Ok(rt.call("session/compact", compact_params(session_id), 120).await?)
}

/// session/goal leitura/controle sem texto (show|pause|resume|clear).
/// set/replace EXCLUÍDOS: campo de texto não verificado (ver goal_params).
pub async fn goal_action(rt: &Arc<Transport>, session_id: &str, action: &str) -> Result<Value, SessionError> {
    validate_goal_action(action)?;
    if action == "set" || action == "replace" {
        return Err(SessionError::Protocol(
            "goal set/replace exigem campo de texto ainda não verificado no servidor — sem invenção (sonda futura)".into(),
        ));
    }
    Ok(rt.call("session/goal", goal_params(session_id, action), 30).await?)
}

/// Envia e aguarda a resposta por poll de `session/messages`.
/// Critério original (headless/REPL): 3 polls estáveis com 1s de intervalo.
pub async fn send_and_wait(
    rt: &Arc<Transport>,
    session_id: &str,
    content: &str,
    timeout_secs: u64,
) -> Result<(Value, String), SessionError> {
    send_and_wait_with(rt, session_id, content, timeout_secs, 1000, 3).await
}

/// Variante TUI: com o poller dedicado + gatilho por notificação, o texto já
/// aparece na UI conforme o servidor publica; aqui só precisamos detectar o
/// FIM do turn rápido — 200ms × 2 polls estáveis (~400ms pós-resposta, antes
/// eram ≥3s). Headless/REPL continuam em `send_and_wait` (comportamento
/// original preservado). Alem do poll, devolve o result BRUTO do
/// `session/send` (projection p/ ctx% e activeToolCalls — antes descartado).
pub async fn send_and_wait_fast(
    rt: &Arc<Transport>,
    session_id: &str,
    content: &str,
    timeout_secs: u64,
) -> Result<(Value, String, Value), SessionError> {
    send_and_wait_with_send(rt, session_id, content, timeout_secs, 200, 2).await
}

/// Envia e aguarda por poll: `stable_needed` polls estáveis consecutivos com
/// texto não-vazio do assistant (intervalo `poll_ms`), ou timeout — no
/// timeout sem texto é erro; com texto, retorna o parcial.
pub async fn send_and_wait_with(
    rt: &Arc<Transport>,
    session_id: &str,
    content: &str,
    timeout_secs: u64,
    poll_ms: u64,
    stable_needed: u32,
) -> Result<(Value, String), SessionError> {
    // Headless/REPL mantêm a tupla de 2: a projection do send segue
    // descartada aqui (zero mudança de comportamento).
    send_and_wait_with_send(rt, session_id, content, timeout_secs, poll_ms, stable_needed)
        .await
        .map(|(raw, text, _send)| (raw, text))
}

/// Núcleo comum: idêntico ao `send_and_wait_with`, mas também devolve o
/// result bruto do `session/send` (projection do create/send, plano §3.2/3.4).
pub async fn send_and_wait_with_send(
    rt: &Arc<Transport>,
    session_id: &str,
    content: &str,
    timeout_secs: u64,
    poll_ms: u64,
    stable_needed: u32,
) -> Result<(Value, String, Value), SessionError> {
    let send = send_message(rt, session_id, content).await?;
    let start = std::time::Instant::now();
    let mut stable = 0u32;
    let mut last_text = String::new();
    let mut last_raw = serde_json::json!({});
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
        let msgs = fetch_messages(rt, session_id).await?;
        last_raw = msgs.clone();
        let cur = last_assistant_text(&msgs).unwrap_or_default();
        if !cur.trim().is_empty() && cur == last_text {
            stable += 1;
        } else if cur != last_text {
            stable = 0;
            last_text = cur;
        }
        if stable >= stable_needed && !last_text.trim().is_empty() {
            break;
        }
        if start.elapsed().as_secs() >= timeout_secs {
            if last_text.trim().is_empty() {
                return Err(SessionError::Protocol(format!(
                    "timeout ({timeout_secs}s) sem resposta do assistant"
                )));
            }
            break; // retorna parcial
        }
    }
    Ok((last_raw, last_text, send))
}

// ---------- persistência local JSON ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub updated_at: String,
}

pub fn history_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("zcode-cli")
        .join("history.json")
}

pub fn load_history() -> Vec<HistoryEntry> {
    let p = history_path();
    let Ok(text) = std::fs::read_to_string(&p) else { return Vec::new() };
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save_history(entries: &[HistoryEntry]) -> Result<(), SessionError> {
    let p = history_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SessionError::Io(e.to_string()))?;
    }
    let text = serde_json::to_string_pretty(entries).map_err(|e| SessionError::Io(e.to_string()))?;
    std::fs::write(&p, text).map_err(|e| SessionError::Io(e.to_string()))?;
    Ok(())
}

/// Upsert por sessionId (mantém máx. 200, mais recentes primeiro).
pub fn record_session(entry: HistoryEntry) {
    let mut all = load_history();
    all.retain(|e| e.session_id != entry.session_id);
    all.insert(0, entry);
    all.truncate(200);
    let _ = save_history(&all);
}

pub fn latest_for_workspace(workspace_norm: &str) -> Option<HistoryEntry> {
    load_history().into_iter().find(|e| e.workspace == workspace_norm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_tem_workspace_path_e_key() {
        let p = create_params("C:/proj");
        assert_eq!(p["workspace"]["workspacePath"], "C:/proj");
        assert_eq!(p["workspace"]["workspaceKey"], "C:/proj");
    }

    #[test]
    fn send_usa_content_nao_message() {
        let p = send_params("sess_1", "oi");
        assert_eq!(p["content"], "oi");
        assert!(p.get("message").is_none());
    }

    #[test]
    fn set_model_e_objeto() {
        let p = set_model_params("sess_1", "zai", "glm-5.3");
        assert_eq!(p["model"]["providerId"], "zai");
        assert_eq!(p["model"]["modelId"], "glm-5.3");
        assert!(!p["model"].is_string());
    }

    #[test]
    fn set_mode_e_thought_shapes() {
        assert_eq!(set_mode_params("s", "plan")["mode"], "plan");
        assert_eq!(set_thought_params("s", "max")["thoughtLevel"], "max");
        assert!(validate_mode("invalido").is_err());
        assert!(validate_thought("medio").is_err());
    }

    #[test]
    fn extrai_session_e_texto() {
        let create: Value = serde_json::from_str(
            r#"{"session":{"sessionId":"sess_abc"},"protocol":{"name":"ZCode Protocol","version":1}}"#,
        )
        .unwrap();
        assert_eq!(extract_session_id(&create).unwrap(), "sess_abc");
        assert_eq!(extract_protocol_version(&create), Some(1));
        let msgs: Value = serde_json::from_str(
            r#"{"messages":[{"role":"user","parts":[{"type":"text","text":"oi"}]},{"role":"assistant","parts":[{"type":"text","text":"Canberra."}]}]}"#,
        )
        .unwrap();
        assert_eq!(last_assistant_text(&msgs).unwrap(), "Canberra.");
    }

    #[test]
    fn extrai_shape_real_info_role() {
        // Shape exato do app-server ao vivo (2026-09-08): role em info.role,
        // parts mistos (step-start/reasoning/text/step-finish).
        let msgs: Value = serde_json::from_str(
            r#"{"messages":[
              {"info":{"role":"user"},"parts":[{"type":"text","text":"diga ok"}]},
              {"info":{"role":"assistant"},"parts":[
                {"type":"step-start"},{"type":"reasoning","text":"pensando"},
                {"type":"text","text":"ok"},{"type":"step-finish"}]}]}"#,
        )
        .unwrap();
        let all = extract_text_messages(&msgs);
        assert_eq!(all.len(), 2);
        assert_eq!(all[1], ("assistant".to_string(), "ok".to_string()));
        assert_eq!(last_assistant_text(&msgs).unwrap(), "ok");
    }

    #[test]
    fn display_items_reasoning_antes_do_texto() {
        // Mesmo fixture do shape real: reasoning vira item PRÓPRIO (kind
        // Reasoning) emitido ANTES do item de texto da mesma mensagem.
        let msgs: Value = serde_json::from_str(
            r#"{"messages":[
              {"info":{"role":"user"},"parts":[{"type":"text","text":"diga ok"}]},
              {"info":{"role":"assistant"},"parts":[
                {"type":"step-start"},{"type":"reasoning","text":"pensando"},
                {"type":"reasoning","text":" mais"},
                {"type":"text","text":"ok"},{"type":"step-finish"}]}]}"#,
        )
        .unwrap();
        let items = extract_display_items(&msgs);
        assert_eq!(
            items,
            vec![
                MsgItem { role: "user".into(), text: "diga ok".into(), kind: MsgKind::Text },
                MsgItem { role: "assistant".into(), text: "pensando mais".into(), kind: MsgKind::Reasoning },
                MsgItem { role: "assistant".into(), text: "ok".into(), kind: MsgKind::Text },
            ]
        );
        // Wrapper preserva o comportamento antigo (só texto, sem reasoning).
        let so_texto = extract_text_messages(&msgs);
        assert_eq!(
            so_texto,
            vec![
                ("user".to_string(), "diga ok".to_string()),
                ("assistant".to_string(), "ok".to_string()),
            ]
        );
    }

    #[test]
    fn display_items_casos_defensivos() {
        // Reasoning sem texto → só o item Reasoning; reasoning vazio → nada;
        // sem parts → nada; raiz-array também é aceita (mesma tolerância do
        // wrapper original).
        let msgs: Value = serde_json::json!({"messages":[
            {"info":{"role":"assistant"},"parts":[{"type":"reasoning","text":"só penso"}]},
            {"info":{"role":"assistant"},"parts":[{"type":"reasoning","text":"   "}]},
            {"info":{"role":"user"}},
            {"info":{"role":"assistant"},"parts":[{"type":"step-start"}]}
        ]});
        let items = extract_display_items(&msgs);
        assert_eq!(
            items,
            vec![MsgItem {
                role: "assistant".into(),
                text: "só penso".into(),
                kind: MsgKind::Reasoning,
            }]
        );
        assert!(extract_display_items(&serde_json::json!({})).is_empty());
        assert!(extract_display_items(&serde_json::json!([])).is_empty());
        // Kind default é Text.
        assert_eq!(MsgKind::default(), MsgKind::Text);
    }

    #[test]
    fn summarize_active_tools_defensivo() {
        // Shape esperado (array de objetos com name) na raiz e na projection.
        let raiz = serde_json::json!({"activeToolCalls":[
            {"name":"bash"},{"name":"bash"},{"name":"edit"}
        ]});
        assert_eq!(
            summarize_active_tools(&raiz).unwrap(),
            "tools neste turno: bash ×2, edit ×1"
        );
        let proj = serde_json::json!({"projection":{"activeToolCalls":[
            {"tool":"read"},{"name":"bash"}
        ]}});
        assert_eq!(
            summarize_active_tools(&proj).unwrap(),
            "tools neste turno: read ×1, bash ×1"
        );
        // Qualquer shape diferente → None (nada emitido, nada panica).
        assert_eq!(summarize_active_tools(&serde_json::json!({})), None);
        assert_eq!(
            summarize_active_tools(&serde_json::json!({"activeToolCalls":[]})),
            None
        );
        assert_eq!(
            summarize_active_tools(&serde_json::json!({"activeToolCalls":["bash"]})),
            None
        );
        assert_eq!(
            summarize_active_tools(&serde_json::json!({"activeToolCalls":[{"tool":42}]})),
            None
        );
        assert_eq!(
            summarize_active_tools(&serde_json::json!({"activeToolCalls":[{"name":"bash"},{"name":""}]})),
            None
        );
        // Campo ativo fora de activeToolCalls não vira falso positivo.
        assert_eq!(
            summarize_active_tools(&serde_json::json!({"name":"bash"})),
            None
        );
    }

    #[test]
    fn modelo_efetivo_vem_do_servidor() {
        let v: Value = serde_json::from_str(
            r#"{"settings":{"model":{"current":{"providerId":"zai","modelId":"glm-5.3-Flash"}}}}"#,
        )
        .unwrap();
        assert_eq!(effective_model(&v), "zai/glm-5.3-Flash");
        assert_eq!(effective_model(&serde_json::json!({})), config::default_model());
    }

    #[test]
    fn tui_visual_turn_stats_tok_s() {
        // TUI statusbar: tok/s sobre output_tokens do delta + tempo medido.
        let before = Usage { input_tokens: 100, output_tokens: 10, total_tokens: 110, ..Usage::default() };
        let after = Usage { input_tokens: 150, output_tokens: 110, total_tokens: 260, ..Usage::default() };
        let st = measure_turn(&before, &after, std::time::Duration::from_secs(2));
        assert_eq!(st, TurnStats { input_tokens: 50, output_tokens: 100, total_tokens: 150, elapsed_ms: 2000, tok_per_s: 50.0 });
    }

    #[test]
    fn tui_visual_turn_stats_limites() {
        // elapsed zero → tok/s 0 (sem divisão por zero); delta invertido satura.
        let a = Usage { output_tokens: 50, ..Usage::default() };
        let b = Usage { output_tokens: 30, ..Usage::default() };
        let st = measure_turn(&a, &b, std::time::Duration::ZERO);
        assert_eq!(st.output_tokens, 0);
        assert_eq!(st.tok_per_s, 0.0);
        assert_eq!(st.elapsed_ms, 0);
    }

    #[test]
    fn tui_visual_contexto_presente() {
        // Fixture no shape do plano §3.4 (projection do create).
        let v: Value = serde_json::from_str(
            r#"{"projection":{"status":"idle","contextWindow":1000000,"contextUsed":250000,"totalTokenCount":250000}}"#,
        )
        .unwrap();
        let c = parse_context(&v);
        assert_eq!(c, ContextUsage { window: 1_000_000, used: 250_000 });
        assert_eq!(c.pct(), 25.0);
        // Aceita o objeto projection direto.
        assert_eq!(parse_context(&v["projection"]), c);
    }

    #[test]
    fn tui_visual_contexto_ausente_nao_quebra() {
        assert_eq!(parse_context(&serde_json::json!({})), ContextUsage::default());
        assert_eq!(parse_context(&serde_json::json!({"projection":{}})).pct(), 0.0);
        let v: Value = serde_json::from_str(r#"{"session":{"sessionId":"s"}}"#).unwrap();
        assert_eq!(parse_context(&v).window, 0);
    }

    #[test]
    fn parse_usage_camel() {
        let v: Value = serde_json::from_str(
            r#"{"totalTokens":10,"inputTokens":7,"outputTokens":3,"reasoningTokens":1,"cacheCreationTokens":0,"cacheReadTokens":2,"modelRequestCount":1}"#,
        )
        .unwrap();
        let u = parse_usage(&v);
        assert_eq!(u.input_tokens, 7);
        assert_eq!(u.total_tokens, 10);
    }

    #[test]
    fn list_e_outros_shapes() {
        assert_eq!(list_params(50)["limit"], 50);
        assert_eq!(usage_params("s")["sessionId"], "s");
        assert_eq!(resume_params("s")["sessionId"], "s");
        assert_eq!(stop_params("s")["sessionId"], "s");
        assert_eq!(fork_params("s")["sessionId"], "s");
    }

    #[test]
    fn stop_mid_turn_tem_session_id() {
        // Fase 3 item 4: stop chamável mid-turn — builder mínimo conferido.
        let p = stop_params("sess_x");
        assert_eq!(p["sessionId"], "sess_x");
        assert_eq!(p.as_object().unwrap().len(), 1);
    }

    #[test]
    fn compact_shape_minimo_verificado() {
        // Evidência live 2026-09-08 (Fase 3): `{}` → -32602 pedindo só sessionId.
        let p = compact_params("sess_x");
        assert_eq!(p["sessionId"], "sess_x");
        assert_eq!(p.as_object().unwrap().len(), 1);
    }

    #[test]
    fn goal_action_enum_verificada() {
        // Enum live 2026-09-08 (Fase 3): show|set|replace|pause|resume|clear.
        for a in ["show", "set", "replace", "pause", "resume", "clear"] {
            assert!(validate_goal_action(a).is_ok(), "{a}");
            let p = goal_params("sess_x", a);
            assert_eq!(p["sessionId"], "sess_x");
            assert_eq!(p["action"], a);
        }
        assert!(validate_goal_action("delete").is_err());
        assert!(validate_goal_action("").is_err());
    }

    #[test]
    fn available_models_do_create() {
        // Fixture no shape real do create (settings.model.available[]).
        let v: Value = serde_json::from_str(
            r#"{"settings":{"model":{
              "current":{"providerId":"zai","modelId":"glm-5.3-Flash"},
              "available":[
                {"label":"glm-5.3-Flash","providerLabel":"Z.AI","ref":{"providerId":"zai","modelId":"glm-5.3-Flash"},"contextWindow":1000000,"maxOutputTokens":128000},
                {"label":"main","ref":{"providerId":"zai","modelId":"glm-5.3"},"contextWindow":1000000}
              ]}}}"#,
        )
        .unwrap();
        let ms = available_models(&v);
        assert_eq!(ms.len(), 2);
        assert_eq!(ms[0], AvailableModel {
            label: "glm-5.3-Flash".into(), provider_id: "zai".into(),
            model_id: "glm-5.3-Flash".into(), context_window: 1_000_000,
        });
        assert_eq!(effective_model(&v), "zai/glm-5.3-Flash");
        assert!(available_models(&serde_json::json!({})).is_empty());
    }

    // ----- /todos: parser defensivo (plano §3.4: todos/todoGroups) -----

    fn td(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem { content: content.into(), status }
    }

    #[test]
    fn todos_array_de_objetos_com_status() {
        // Shape canônico: result.todos = [{content, status}].
        let v = serde_json::json!({"todos": [
            {"content": "ler plano", "status": "completed"},
            {"content": "implementar", "status": "in-progress"},
            {"content": "testar", "status": "pending"}
        ]});
        assert_eq!(
            extract_todos(&v),
            vec![
                td("ler plano", TodoStatus::Completed),
                td("implementar", TodoStatus::InProgress),
                td("testar", TodoStatus::Pending),
            ]
        );
        // Grafias alternativas: done/in_progress/cancelled/desconhecido.
        let v2 = serde_json::json!({"todos": [
            {"content": "a", "status": "done"},
            {"content": "b", "status": "in_progress"},
            {"content": "c", "status": "cancelled"},
            {"content": "d", "status": "que-status-e-esse"}
        ]});
        assert_eq!(
            extract_todos(&v2),
            vec![
                td("a", TodoStatus::Completed),
                td("b", TodoStatus::InProgress),
                td("c", TodoStatus::Pending),
                td("d", TodoStatus::Pending),
            ]
        );
    }

    #[test]
    fn todos_campos_alternativos_text_label_state() {
        // content/text/label e status/state — primeiro não-vazio vence.
        let v = serde_json::json!({"todos": [
            {"text": "via text", "state": "completed"},
            {"label": "via label"},
            {"content": "  ", "text": "content vazio cai no text"},
            {"content": "sem status algum"}
        ]});
        assert_eq!(
            extract_todos(&v),
            vec![
                td("via text", TodoStatus::Completed),
                td("via label", TodoStatus::Pending),
                td("content vazio cai no text", TodoStatus::Pending),
                td("sem status algum", TodoStatus::Pending),
            ]
        );
    }

    #[test]
    fn todos_array_de_strings_vira_pending() {
        let v = serde_json::json!({"todos": ["item um", "item dois", "  "]});
        assert_eq!(
            extract_todos(&v),
            vec![td("item um", TodoStatus::Pending), td("item dois", TodoStatus::Pending)]
        );
    }

    #[test]
    fn todos_da_projection_do_send() {
        // Fim de turno: o send empacota o estado na projection.
        let v = serde_json::json!({"projection": {"todos": [
            {"content": "passo", "status": "in-progress"}
        ]}});
        assert_eq!(extract_todos(&v), vec![td("passo", TodoStatus::InProgress)]);
        // Raiz tem precedência sobre a projection.
        let ambos = serde_json::json!({
            "todos": [{"content": "da raiz", "status": "completed"}],
            "projection": {"todos": [{"content": "da proj"}]}
        });
        assert_eq!(extract_todos(&ambos), vec![td("da raiz", TodoStatus::Completed)]);
    }

    #[test]
    fn todo_groups_achatado_tolerante() {
        // Grupo com lista interna (todos/items/tasks) + título ignorado.
        let v = serde_json::json!({"todoGroups": [
            {"title": "fase 1", "todos": [
                {"content": "a", "status": "completed"},
                {"content": "b", "status": "in-progress"}
            ]},
            {"name": "fase 2", "items": [{"content": "c"}]}
        ]});
        assert_eq!(
            extract_todos(&v),
            vec![
                td("a", TodoStatus::Completed),
                td("b", TodoStatus::InProgress),
                td("c", TodoStatus::Pending),
            ]
        );
        // Grupo sem lista interna: string ou objeto direto vira item.
        let v2 = serde_json::json!({"todoGroups": [
            "item solto",
            {"content": "objeto direto", "status": "done"}
        ]});
        assert_eq!(
            extract_todos(&v2),
            vec![td("item solto", TodoStatus::Pending), td("objeto direto", TodoStatus::Completed)]
        );
        // todoGroups dentro da projection também conta.
        let v3 = serde_json::json!({"projection": {"todoGroups": [{"todos": ["x"]}]}});
        assert_eq!(extract_todos(&v3), vec![td("x", TodoStatus::Pending)]);
    }

    #[test]
    fn todos_shapes_hostis_sem_panic() {
        // Array vazio, campo string pura, número, null, objeto sem conteúdo.
        assert!(extract_todos(&serde_json::json!({"todos": []})).is_empty());
        assert!(extract_todos(&serde_json::json!({"todos": "não sou array"})).is_empty());
        assert!(extract_todos(&serde_json::json!({"todos": [42, null, {}]})).is_empty());
        assert!(extract_todos(&serde_json::json!({"todos": [{"status": "completed"}]})).is_empty());
        // Result inteiro string/number/nulo e objeto sem campos conhecidos.
        assert!(extract_todos(&serde_json::json!("string pura")).is_empty());
        assert!(extract_todos(&serde_json::json!(7)).is_empty());
        assert!(extract_todos(&Value::Null).is_empty());
        assert!(extract_todos(&serde_json::json!({"session": {"sessionId": "s"}})).is_empty());
        assert!(extract_todos(&serde_json::json!({"todoGroups": []})).is_empty());
        assert!(extract_todos(&serde_json::json!({"todoGroups": [{"title": "sem lista"}]})).is_empty());
        // Objeto com status estranho DENTRO de item válido → Pending (não descarta o item).
        let v = serde_json::json!({"todos": [{"content": "ok", "status": 123}]});
        assert_eq!(extract_todos(&v), vec![td("ok", TodoStatus::Pending)]);
    }

    // ----- /export: builders puros (Fase V4-2) -----

    #[test]
    fn export_sid_short8() {
        assert_eq!(sid_short8("sess_abff123456789"), "sess_abf");
        assert_eq!(sid_short8("curta"), "curta");
        assert_eq!(sid_short8(""), "");
    }

    #[test]
    fn export_md_cabecalho_secoes_e_thinking() {
        let msgs = vec![
            ("user".to_string(), "qual é a capital?".to_string(), MsgKind::Text),
            ("assistant".to_string(), "pensando\nmais uma linha".to_string(), MsgKind::Reasoning),
            ("assistant".to_string(), "Canberra.".to_string(), MsgKind::Text),
            ("system".to_string(), "notice do boot".to_string(), MsgKind::Text),
        ];
        let md = build_export_md(&msgs, "sess_abff123456789", "zai/glm-5.3-Flash", "2026-09-08T12:00:00+00:00");
        // Cabeçalho: sid truncado em 8 + modelo + data RFC3339.
        assert!(md.starts_with("# Conversa zcode-cli (sessão sess_abf, modelo zai/glm-5.3-Flash, 2026-09-08T12:00:00+00:00)\n\n"), "{md}");
        // Uma seção por mensagem com o badge do role.
        assert!(md.contains("## USER\n\nqual é a capital?\n"));
        assert!(md.contains("## ASSIST\n\nCanberra.\n"));
        assert!(md.contains("## SYS\n\nnotice do boot\n"));
        // Reasoning: bloco quote marcado com "> (thinking)" + linhas citadas.
        assert!(md.contains("## THINK\n\n> (thinking)\n> pensando\n> mais uma linha\n"));
        // Modelo vazio degrada p/ "-" sem quebrar o formato.
        let md2 = build_export_md(&[], "", "", "2026-09-08T12:00:00+00:00");
        assert!(md2.contains("modelo -, 2026-09-08T12:00:00+00:00"));
        assert!(md2.contains("(sem mensagens)"));
    }

    #[test]
    fn export_json_shape_e_escaping_do_serde() {
        let msgs = vec![
            ("user".to_string(), "com \"aspas\" e \\ barra".to_string(), MsgKind::Text),
            ("assistant".to_string(), "raciocínio".to_string(), MsgKind::Reasoning),
        ];
        let v = build_export_json(&msgs, "sess_abff123456789", "zai/glm-5.3-Flash", "2026-09-08T12:00:00+00:00");
        assert_eq!(v["sessionId"], "sess_abff123456789");
        assert_eq!(v["model"], "zai/glm-5.3-Flash");
        assert_eq!(v["exportedAt"], "2026-09-08T12:00:00+00:00");
        let arr = v["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[0]["kind"], "text");
        assert_eq!(arr[0]["text"], "com \"aspas\" e \\ barra");
        assert_eq!(arr[1]["kind"], "reasoning");
        // Round-trip pelo serde: escaping correto (texto cru sobrevive).
        let s = serde_json::to_string(&v).unwrap();
        let volta: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(volta["messages"][0]["text"], "com \"aspas\" e \\ barra");
    }
}
