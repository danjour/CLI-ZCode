//! Sessões: builders de params (testáveis), parsing de results, ops async e
//! persistência local JSON (escolha documentada em `config.rs`).
//!
//! Persistência: `~/.local/share/zcode-cli/history.json`
//! `[{sessionId, title, workspace, model, updatedAt}]`.

use crate::config;
use crate::runtime::Runtime;
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

// ---------- ops async ----------

pub async fn create_session(rt: &Arc<Runtime>, workspace: &str) -> Result<Value, SessionError> {
    let res = rt.call("session/create", create_params(workspace), 60).await?;
    if let Some(v) = extract_protocol_version(&res) {
        if config::is_new_protocol_version(Some(v)) {
            tracing::warn!(version = v, "protocol.version > 1 — chamar orquestrador (shape pode ter mudado)");
        }
    }
    Ok(res)
}

pub async fn send_message(rt: &Arc<Runtime>, session_id: &str, content: &str) -> Result<Value, SessionError> {
    Ok(rt.call("session/send", send_params(session_id, content), 120).await?)
}

pub async fn fetch_messages(rt: &Arc<Runtime>, session_id: &str) -> Result<Value, SessionError> {
    Ok(rt.call("session/messages", messages_params(session_id), 30).await?)
}

pub async fn list_sessions(rt: &Arc<Runtime>, limit: u32) -> Result<Value, SessionError> {
    Ok(rt.call("session/list", list_params(limit), 30).await?)
}

pub async fn fetch_usage(rt: &Arc<Runtime>, session_id: &str) -> Result<Usage, SessionError> {
    let v = rt.call("session/usage", usage_params(session_id), 30).await?;
    Ok(parse_usage(&v))
}

pub async fn apply_model(rt: &Arc<Runtime>, session_id: &str, model: &str) -> Result<Value, SessionError> {
    let (p, m) = config::split_model_ref(model).map_err(|e| SessionError::Config(e.to_string()))?;
    Ok(rt.call("session/setModel", set_model_params(session_id, &p, &m), 30).await?)
}

pub async fn apply_mode(rt: &Arc<Runtime>, session_id: &str, mode: &str) -> Result<Value, SessionError> {
    validate_mode(mode)?;
    Ok(rt.call("session/setMode", set_mode_params(session_id, mode), 30).await?)
}

pub async fn apply_thought(rt: &Arc<Runtime>, session_id: &str, level: &str) -> Result<Value, SessionError> {
    validate_thought(level)?;
    Ok(rt.call("session/setThoughtLevel", set_thought_params(session_id, level), 30).await?)
}

/// session/stop mid-turn (Fase 3: Ctrl+C durante turno → stop + volta ao prompt).
/// Builder já existia; op exposta para o wiring da Maria (sem UX aqui).
pub async fn stop_turn(rt: &Arc<Runtime>, session_id: &str) -> Result<Value, SessionError> {
    Ok(rt.call("session/stop", stop_params(session_id), 15).await?)
}

/// session/compact (Fase 3). CUIDADO: em sessão real dispara sumarização via LLM
/// (gasta plano) — a Maria deve confirmar antes no /compact.
pub async fn compact_session(rt: &Arc<Runtime>, session_id: &str) -> Result<Value, SessionError> {
    Ok(rt.call("session/compact", compact_params(session_id), 120).await?)
}

/// session/goal leitura/controle sem texto (show|pause|resume|clear).
/// set/replace EXCLUÍDOS: campo de texto não verificado (ver goal_params).
pub async fn goal_action(rt: &Arc<Runtime>, session_id: &str, action: &str) -> Result<Value, SessionError> {
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
    rt: &Arc<Runtime>,
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
    rt: &Arc<Runtime>,
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
    rt: &Arc<Runtime>,
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
    rt: &Arc<Runtime>,
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
}
