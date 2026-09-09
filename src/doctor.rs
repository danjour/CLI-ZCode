//! doctor — diagnóstico 100% local, zero sessão (Fase 6).
//!
//! Verifica sem gastar plano: node no PATH, `zcode.cjs` (todas as rotas +
//! overrides), TOML parseável, modelos locais (só nomes — NUNCA segredos).
//! Exit 0 se OK (avisos tolerados), 1 se algum check falhar.

use crate::cli::Cli;
use crate::commands::CmdError;
use crate::config;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn tag(self) -> &'static str {
        match self {
            Status::Ok => "OK  ",
            Status::Warn => "AVISO",
            Status::Fail => "FALHA",
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

// ---------- partes puras (testáveis, fixtures) ----------

/// "v24.13.1\n" → Some("v24.13.1"); lixo → None.
pub fn parse_node_version(stdout: &str) -> Option<String> {
    let v = stdout.trim();
    if v.len() > 1 && (v.starts_with('v') || v.as_bytes()[0].is_ascii_digit()) {
        Some(v.to_string())
    } else {
        None
    }
}

/// Estado do TOML: ausente → Warn (defaults); ok → Ok; quebrado → Fail.
pub fn check_toml_text(text: Option<&str>) -> Check {
    match text {
        None => Check {
            name: "config",
            status: Status::Warn,
            detail: format!("sem {} — usando defaults", config::config_path().to_string_lossy()),
        },
        Some(t) => match config::parse_toml_str(t) {
            Ok(_) => Check {
                name: "config",
                status: Status::Ok,
                detail: config::config_path().to_string_lossy().to_string(),
            },
            Err(e) => Check {
                name: "config",
                status: Status::Fail,
                detail: format!("TOML inválido: {e}"),
            },
        },
    }
}

/// Extrai SÓ nomes de modelos/providers do config.json da GUI.
/// Nunca retorna segredos (options/apiKey ignorados por construção).
/// Esperado: {"model":{"main":"zai/x","lite":"zai/y"},"provider":{"zai":{...}}}.
pub fn parse_gui_models(text: &str) -> Result<Vec<String>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    if let Some(m) = v.get("model") {
        for k in ["main", "lite"] {
            if let Some(s) = m.get(k).and_then(|x| x.as_str()) {
                out.push(format!("model.{k}={s}"));
            }
        }
    }
    if let Some(p) = v.get("provider").and_then(|x| x.as_object()) {
        let mut names: Vec<String> = p.keys().cloned().collect();
        names.sort();
        for n in names {
            out.push(format!("provider.{n}"));
        }
    }
    if out.is_empty() {
        return Err("sem model.* nem provider.* legíveis".to_string());
    }
    Ok(out)
}

pub fn check_gui_models(text: Option<&str>) -> Check {
    match text {
        None => Check {
            name: "modelos",
            status: Status::Warn,
            detail: "sem ~/.zcode/cli/config.json — /model usa effective_model da sessão".to_string(),
        },
        Some(t) => match parse_gui_models(t) {
            Ok(ms) => Check { name: "modelos", status: Status::Ok, detail: ms.join(", ") },
            Err(e) => Check { name: "modelos", status: Status::Warn, detail: format!("ilegível ({e})") },
        },
    }
}

/// Exit geral: qualquer Fail → 1, senão 0 (avisos tolerados).
pub fn overall_exit(checks: &[Check]) -> i32 {
    if checks.iter().any(|c| c.status == Status::Fail) {
        1
    } else {
        0
    }
}

pub fn format_human(checks: &[Check]) -> String {
    let mut s = String::from("zcode-cli doctor\n");
    for c in checks {
        s.push_str(&format!("  [{}] {:<9} {}\n", c.status.tag(), c.name, c.detail));
    }
    s
}

pub fn format_json(checks: &[Check]) -> Value {
    let items: Vec<Value> = checks
        .iter()
        .map(|c| serde_json::json!({"check": c.name, "status": c.status.as_str(), "detail": c.detail}))
        .collect();
    serde_json::json!({ "ok": overall_exit(checks) == 0, "checks": items })
}

// ---------- coleta real (só leitura local, sem sessão) ----------

async fn check_node(node_bin: &str) -> Check {
    let out = tokio::process::Command::new(node_bin).arg("--version").output().await;
    match out {
        Ok(o) if o.status.success() => {
            match parse_node_version(&String::from_utf8_lossy(&o.stdout)) {
                Some(v) => Check { name: "node", status: Status::Ok, detail: format!("{node_bin} {v}") },
                None => Check { name: "node", status: Status::Fail, detail: format!("{node_bin} respondeu lixo") },
            }
        }
        _ => Check {
            name: "node",
            status: Status::Fail,
            detail: format!("node não encontrado no PATH (node={node_bin}). Instale o Node.js."),
        },
    }
}

fn check_zcode(explicit: Option<&str>, cfg: &config::FileConfig) -> Check {
    // Rótulo da rota vencedora (sem duplicar a busca do config::discover).
    let route = if explicit.is_some_and(|s| !s.is_empty()) {
        "override --runtime"
    } else if std::env::var("ZCODE_CJS").is_ok_and(|v| !v.trim().is_empty()) {
        "env ZCODE_CJS"
    } else if !cfg.runtime.zcode_cjs.trim().is_empty() {
        "config TOML"
    } else {
        "autodetecção"
    };
    match config::discover_zcode_cjs(explicit, cfg) {
        Ok(p) => Check { name: "zcode.cjs", status: Status::Ok, detail: format!("{} ({route})", p.to_string_lossy()) },
        Err(e) => Check { name: "zcode.cjs", status: Status::Fail, detail: e.to_string() },
    }
}

fn gui_models_path() -> std::path::PathBuf {
    dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from(".")).join(".zcode").join("cli").join("config.json")
}

/// Executa o doctor: imprime (humano ou --json) e retorna Err se houver falha.
/// NUNCA imprime segredos: só nomes de checks + derivados não-sensíveis.
pub async fn run(cli: &Cli) -> Result<(), CmdError> {
    let cfg = config::load_config();
    let node = check_node(&cfg.runtime.node).await;
    let zcode = check_zcode(cli.runtime.as_deref(), &cfg);
    let toml_text = std::fs::read_to_string(config::config_path()).ok();
    let toml = check_toml_text(toml_text.as_deref());
    let gui_text = std::fs::read_to_string(gui_models_path()).ok();
    let models = check_gui_models(gui_text.as_deref());
    let exe = match std::env::current_exe() {
        Ok(p) => Check { name: "binário", status: Status::Ok, detail: p.to_string_lossy().to_string() },
        Err(e) => Check { name: "binário", status: Status::Warn, detail: e.to_string() },
    };
    let checks = vec![node, zcode, toml, models, exe];
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&format_json(&checks)).unwrap_or_else(|_| "{}".into()));
    } else {
        print!("{}", format_human(&checks));
    }
    if overall_exit(&checks) == 0 {
        Ok(())
    } else {
        Err(CmdError::Config("doctor: há falhas acima (exit 1)".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_version_parse() {
        assert_eq!(parse_node_version("v24.13.1\n"), Some("v24.13.1".into()));
        assert_eq!(parse_node_version("22.1.0"), Some("22.1.0".into()));
        assert!(parse_node_version("").is_none());
        assert!(parse_node_version("erro feio {{").is_none());
    }

    #[test]
    fn toml_estados() {
        assert_eq!(check_toml_text(None).status, Status::Warn);
        let ok = check_toml_text(Some("[default]\nmode = \"build\"\n"));
        assert_eq!(ok.status, Status::Ok);
        let bad = check_toml_text(Some("[default\nmode = = ="));
        assert_eq!(bad.status, Status::Fail);
        assert!(bad.detail.contains("TOML inválido"));
    }

    #[test]
    fn modelos_gui_sem_segredos() {
        // Fixture no shape real; segredo presente mas NUNCA sai no output.
        let fixture = r#"{"model":{"main":"zai/glm-5.3-Flash","lite":"zai/glm-5.3-Flash"},
            "provider":{"zai":{"kind":"anthropic","options":{"apiKey":"SEGREDO","baseURL":"https://x"}}}}"#;
        let ms = parse_gui_models(fixture).unwrap();
        let joined = ms.join(" ");
        assert!(joined.contains("model.main=zai/glm-5.3-Flash"));
        assert!(joined.contains("provider.zai"));
        assert!(!joined.contains("SEGREDO"));
        assert!(!joined.contains("apiKey"));
        assert!(parse_gui_models("{}").is_err());
        assert!(parse_gui_models("lixo").is_err());
        // check_* nunca vaza segredo mesmo com fixture hostil.
        let c = check_gui_models(Some(fixture));
        assert_eq!(c.status, Status::Ok);
        assert!(!c.detail.contains("SEGREDO"));
        assert_eq!(check_gui_models(None).status, Status::Warn);
    }

    #[test]
    fn exit_e_formatos() {
        let ok = vec![Check { name: "a", status: Status::Ok, detail: "x".into() }];
        let warn = vec![Check { name: "a", status: Status::Warn, detail: "x".into() }];
        let fail = vec![
            Check { name: "a", status: Status::Ok, detail: "x".into() },
            Check { name: "b", status: Status::Fail, detail: "y".into() },
        ];
        assert_eq!(overall_exit(&ok), 0);
        assert_eq!(overall_exit(&warn), 0); // avisos tolerados
        assert_eq!(overall_exit(&fail), 1);
        let h = format_human(&fail);
        assert!(h.contains("FALHA") && h.contains("zcode-cli doctor"));
        let j = format_json(&fail);
        assert_eq!(j["ok"], false);
        assert_eq!(j["checks"][1]["status"], "fail");
    }
}
