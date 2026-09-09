//! Config: TOML + descoberta do `zcode.cjs` + normalização de paths.
//!
//! Arquivo: `~/.config/zcode-cli/config.toml` (ver plano §4).
//! Persistência da escolha: JSON (sem rusqlite) — ver `session.rs` (HistoryStore).
//! Motivo: zero dependência nativa no Windows, diff mínimo, suficiente p/ Fase 2
//! (sessionId, título, workspace, modelo). Migrável p/ sqlite depois sem quebrar CLI.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{0}")]
    NotFound(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileConfig {
    #[serde(default)]
    pub runtime: RuntimeCfg,
    #[serde(default)]
    pub default: DefaultCfg,
    #[serde(default)]
    pub ui: UiCfg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeCfg {
    #[serde(default)]
    pub zcode_cjs: String,
    #[serde(default = "default_node")]
    pub node: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefaultCfg {
    #[serde(default = "default_workspace")]
    pub workspace: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_thought")]
    pub thought_level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiCfg {
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_refresh")]
    pub stream_refresh_ms: u64,
}

fn default_node() -> String {
    "node".to_string()
}
fn default_workspace() -> String {
    current_dir_forward()
}
fn default_mode() -> String {
    "build".to_string()
}
/// Modelo default do plano §3.3/§4.
/// DIVERGÊNCIA VERIFICADA 2026-09-08: o `app-server` ao vivo rejeita
/// `session/setModel {zai, glm-5.3}` ("Available models: main, zai/glm-5.3-Flash");
/// o modelo corrente real é `zai/glm-5.3-Flash`. Por isso o CLI NÃO aplica
/// setModel automaticamente — só com `--model` explícito (ver commands.rs).
pub fn default_model() -> String {
    "zai/glm-5.3".to_string()
}
fn default_model_cfg() -> String {
    default_model()
}
fn default_thought() -> String {
    "max".to_string()
}
fn default_theme() -> String {
    "dark".to_string()
}
fn default_refresh() -> u64 {
    // Intervalo de poll do transcript da TUI ENQUANTO há turn ativo (working);
    // ocioso usa cadence longo fixo (commands::poll_interval_ms). Default
    // reduzido 500→300 para fluidez; configs antigas continuam válidas e o
    // mínimo continua clampado em 100ms.
    300
}

impl Default for RuntimeCfg {
    fn default() -> Self {
        Self { zcode_cjs: String::new(), node: default_node() }
    }
}
impl Default for DefaultCfg {
    fn default() -> Self {
        Self {
            workspace: default_workspace(),
            mode: default_mode(),
            model: default_model_cfg(),
            thought_level: default_thought(),
        }
    }
}
impl Default for UiCfg {
    fn default() -> Self {
        Self { theme: default_theme(), stream_refresh_ms: default_refresh() }
    }
}
impl Default for FileConfig {
    fn default() -> Self {
        Self { runtime: RuntimeCfg::default(), default: DefaultCfg::default(), ui: UiCfg::default() }
    }
}

fn current_dir_forward() -> String {
    std::env::current_dir()
        .map(|p| normalize_workspace(&p.to_string_lossy()))
        .unwrap_or_else(|_| "C:/Users/eduar".to_string())
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("zcode-cli")
        .join("config.toml")
}

/// Carrega o TOML se existir, senão defaults. Nunca falha por arquivo ausente.
pub fn load_config() -> FileConfig {
    let path = config_path();
    let Ok(text) = std::fs::read_to_string(&path) else { return FileConfig::default() };
    toml::from_str(&text).unwrap_or_default()
}

/// Parse puro do TOML (para o doctor distinguir ausente × quebrado).
pub fn parse_toml_str(text: &str) -> Result<FileConfig, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

/// Descoberta do `zcode.cjs` (plano §4).
/// Ordem: override explícito (--runtime) > env ZCODE_CJS > config TOML >
/// %LOCALAPPDATA% (Windows) > macOS > Linux.
pub fn discover_zcode_cjs(explicit: Option<&str>, cfg: &FileConfig) -> Result<PathBuf, ConfigError> {
    // Override explícito (--runtime): erro duro se não existir — sem fallback
    // silencioso (critério "pasta sem ZCode dá erro claro").
    if let Some(e) = explicit.filter(|s| !s.is_empty()) {
        let p = PathBuf::from(e);
        if p.is_file() {
            return Ok(p);
        }
        return Err(ConfigError::NotFound(format!(
            "ZCode não encontrado em {e} (via --runtime). Instale o ZCode desktop ou corrija o caminho."
        )));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(env) = std::env::var("ZCODE_CJS") {
        if !env.trim().is_empty() {
            candidates.push(PathBuf::from(env));
        }
    }
    if !cfg.runtime.zcode_cjs.trim().is_empty() {
        candidates.push(PathBuf::from(cfg.runtime.zcode_cjs.clone()));
    }
    // Windows
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local).join("Programs").join("ZCode").join("resources").join("glm").join("zcode.cjs"),
        );
    }
    // macOS
    candidates.push(PathBuf::from("/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs"));
    // Linux
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/share/ZCode/resources/glm/zcode.cjs"));
    }
    candidates.push(PathBuf::from("/opt/ZCode/resources/glm/zcode.cjs"));

    for c in &candidates {
        if c.is_file() {
            return Ok(c.clone());
        }
    }
    Err(ConfigError::NotFound(format!(
        "ZCode não encontrado em {}. Instale o ZCode desktop ou informe --runtime / ZCODE_CJS.",
        candidates
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("; ")
    )))
}

/// Normaliza workspace p/ o runtime: aceita `C:\` e `C:/`, vira forward slash.
pub fn normalize_workspace(input: &str) -> String {
    let t = input.trim().trim_matches('"').trim();
    // Absolutiza relativo contra cwd quando parece caminho local.
    let abs = if looks_like_path(t) && PathBuf::from(t).is_relative() {
        std::env::current_dir()
            .map(|c| c.join(t).to_string_lossy().to_string())
            .unwrap_or_else(|_| t.to_string())
    } else {
        t.to_string()
    };
    abs.replace('\\', "/")
}

fn looks_like_path(s: &str) -> bool {
    s.contains('/') || s.contains('\\') || s.len() >= 2 && s.as_bytes()[1] == b':' || s.starts_with('.')
}

/// `"zai/glm-5.3"` → (providerId, modelId). Erro se sem `/`.
pub fn split_model_ref(model: &str) -> Result<(String, String), ConfigError> {
    let m = model.trim();
    match m.split_once('/') {
        Some((p, id)) if !p.is_empty() && !id.is_empty() => Ok((p.to_string(), id.to_string())),
        _ => Err(ConfigError::NotFound(format!(
            "modelo inválido '{m}': use providerId/modelId (ex.: zai/glm-5.3)"
        ))),
    }
}

/// Avisa se `protocol.version > 1` (plano §8: protocolo pode mudar).
/// Retorna true se é versão nova (chamar orquestrador).
pub fn is_new_protocol_version(v: Option<u64>) -> bool {
    matches!(v, Some(n) if n > 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normaliza_backslash_windows() {
        assert_eq!(normalize_workspace(r"C:\proj\a"), "C:/proj/a");
        assert_eq!(normalize_workspace("C:/proj/a"), "C:/proj/a");
    }

    #[test]
    fn split_model_ok_e_invalido() {
        assert_eq!(split_model_ref("zai/glm-5.3").unwrap(), ("zai".into(), "glm-5.3".into()));
        assert!(split_model_ref("lite").is_err());
        assert!(split_model_ref("zai/glm-5.3-Flash").unwrap().1 == "glm-5.3-Flash");
    }

    #[test]
    fn versao_nova_detectada() {
        assert!(is_new_protocol_version(Some(2)));
        assert!(!is_new_protocol_version(Some(1)));
        assert!(!is_new_protocol_version(None));
    }

    #[test]
    fn override_inexistente_da_erro_claro() {
        let cfg = FileConfig::default();
        let r = discover_zcode_cjs(Some("C:/caminho/que/nao/existe-xyz/zcode.cjs"), &cfg);
        let e = r.expect_err("--runtime inválido deve falhar sem fallback");
        assert!(e.to_string().contains("ZCode não encontrado"));
    }

    #[test]
    fn default_refresh_300_para_poll_working() {
        // Fase 7 (fluidez): stream_refresh_ms virou o intervalo de poll do
        // transcript ENQUANTO working (ocioso usa cadence longo fixo). Config
        // antiga (ex.: 500) continua válida; o clamp mínimo fica em
        // commands::poll_interval_ms.
        assert_eq!(UiCfg::default().stream_refresh_ms, 300);
        assert_eq!(FileConfig::default().ui.stream_refresh_ms, 300);
        // TOML antigo com 500 parseia intacto (sem migração).
        let old = parse_toml_str("[ui]\nstream_refresh_ms = 500\n").unwrap();
        assert_eq!(old.ui.stream_refresh_ms, 500);
    }
}
