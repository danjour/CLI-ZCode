//! Aviso passivo de nova versão (plano V5-3): 1×/dia, best-effort TOTAL.
//!
//! - Startup (após tracing, antes do dispatch) spawna esta task com orçamento
//!   curto (~1,5s) — nunca atrasa nem falha o comando; se o processo sai antes,
//!   a task morre silenciosamente (é só um aviso).
//! - `update-check.json` no data_dir guarda a última checagem; < 24h → NÃO
//!   checa. O timestamp é gravado em sucesso OU falha de rede — offline não
//!   martela a API a cada comando.
//! - Consulta `releases/latest` da API do GitHub (sem auth) via `curl`
//!   subprocesso: presente no Windows 10+/macOS/distros Linux usuais (os
//!   próprios install.ps1/install.sh já dependem dele) — zero dependência
//!   nova de TLS no binário. Sem curl → silenciosamente não checa.
//! - Comparação semver PURA e tolerante (`version_is_newer`): prefixo `v`,
//!   sufixos sujos (`-rc.1`, HTML) são ignorados; versão malformada → false
//!   (o pior caso é PERDER um aviso, nunca falso positivo).
//! - Saída: UMA linha em STDERR (stdout fica limpo p/ `--json` — que aliás
//!   nem chega aqui: main.rs não spawna a task em `--json`, `daemon` nem TUI).
//! - Erros são absolutamente silenciosos (no máximo tracing::debug no log).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// Intervalo mínimo entre checagens (1×/dia — spec).
pub const INTERVALO_SECS: i64 = 24 * 60 * 60;
/// Orçamento total da task (rede + curl) — nunca atrasa o startup.
const BUDGET: Duration = Duration::from_millis(1500);
/// Orçamento interno do curl (menor que o BUDGET, que cobre o spawn também).
const CURL_MAX_TIME: &str = "1";
const URL: &str = "https://api.github.com/repos/danjour/CLI-ZCode/releases/latest";

// ---------- partes puras (testáveis) ----------

/// Decisão de checar agora? (`now`/`last` injetados, unix segundos).
/// Sem registro anterior → checa; última < 24h atrás → NÃO checa; EXATAMENTE
/// 24h → checa (a spec exclui só o que é MENOR que 24h). `last` no futuro
/// (relógio atrasado) → saturação em 0 → não checa (não martelar).
pub fn deve_checar(now: i64, last: Option<i64>) -> bool {
    match last {
        None => true,
        Some(l) => now.saturating_sub(l) >= INTERVALO_SECS,
    }
}

/// A versão `remote` é ESTRITAMENTE mais nova que `local`? Comparação por
/// componentes numéricos (major.minor.patch), com prefixo `v`/`V` opcional e
/// tolerância a sufixos sujos. Qualquer lado malformado → false.
pub fn version_is_newer(remote: &str, local: &str) -> bool {
    match (parse_version(remote), parse_version(local)) {
        (Some(r), Some(l)) => r > l,
        _ => false,
    }
}

/// "v1.2.3-rc.2" / "1.2.3 (Latest)" → Some((1, 2, 3)). Regras: trim, remove
/// UM prefixo v/V, até 3 componentes separados por '.'; componente PRESENTE
/// precisa ter dígito INICIAL (o resto é sujo/ignorado); componente AUSENTE
/// vale 0 ("1.2" → (1, 2, 0)). Excesso de partes ("1.2.3.4") é ignorado;
/// lixo puro → None.
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let mut partes = s.split('.');
    let pega = |p: Option<&str>| -> Option<u64> {
        let digits: String = p?
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if digits.is_empty() {
            return None;
        }
        digits.parse().ok()
    };
    let a = pega(partes.next())?;
    // Ausente → 0 ("1.2" → (1,2,0)); presente MAS sem dígito inicial
    // ("1.2.x") → None → versão malformada (o chamador trata como false).
    let b = match partes.next() {
        Some(p) => pega(Some(p))?,
        None => 0,
    };
    let c = match partes.next() {
        Some(p) => pega(Some(p))?,
        None => 0,
    };
    Some((a, b, c))
}

/// Extrai `tag_name` do corpo JSON da API (puro). Corpo ilegível/sem campo →
/// None (silêncio).
pub fn extrair_tag(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("tag_name")?.as_str().map(|s| s.to_string())
}

/// A linha do aviso (pura p/ teste): normaliza a tag p/ prefixo `v` e cita o
/// comando de atualização.
pub fn linha_aviso(remote_tag: &str, local: &str) -> String {
    let tag = if remote_tag.starts_with(['v', 'V']) {
        remote_tag.to_string()
    } else {
        format!("v{remote_tag}")
    };
    format!(
        "nova versão disponível: {tag} (instalada: v{local}) — atualize com: \
         cargo install --git https://github.com/danjour/CLI-ZCode"
    )
}

// ---------- estado em disco ----------

/// Registro da última checagem (unix segundos).
#[derive(Debug, Serialize, Deserialize)]
struct Registro {
    #[serde(rename = "lastCheck")]
    last_check: i64,
}

/// Caminho do `update-check.json` (mesmo data_dir do log/daemon.json).
pub fn update_check_path() -> PathBuf {
    crate::data_dir().join("update-check.json")
}

/// Última checagem gravada; arquivo ausente/ilegível → None (checa).
fn ler_last_check(path: &std::path::Path) -> Option<i64> {
    let text = std::fs::read_to_string(path).ok()?;
    let r: Registro = serde_json::from_str(&text).ok()?;
    Some(r.last_check)
}

/// Grava a última checagem — best-effort, erro engolido (aviso nunca falha
/// nada). O diretório pode não existir ainda (cria antes, best-effort).
fn gravar_last_check(path: &std::path::Path, secs: i64) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = serde_json::to_string(&Registro { last_check: secs })
        .unwrap_or_else(|_| "{}".into());
    if let Err(e) = std::fs::write(path, body) {
        tracing::debug!(%e, "update-check: não gravou timestamp");
    }
}

/// "Agora" em unix segundos (relógio do sistema; erro → 0 = checa hoje).
fn agora_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------- rede (best-effort) ----------

/// Corpo bruto de `releases/latest` via curl, ou None em qualquer falha
/// (sem curl, rede lenta, HTTP >= 400 — ex. rate limit). Sem auth (a API
/// aceita anônimo com User-Agent).
async fn buscar() -> Option<String> {
    let out = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "--fail",
            "--max-time",
            CURL_MAX_TIME,
            "-H",
            "User-Agent: zcode-cli",
            URL,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Orquestração (roda em task própria): decide, consulta, avisa e grava o
/// timestamp — nessa ordem, tudo silencioso.
pub async fn run() {
    let path = update_check_path();
    if !deve_checar(agora_secs(), ler_last_check(&path)) {
        return;
    }
    // Orçamento TOTAL: rede não pode passar de BUDGET de jeito nenhum.
    let corpo = tokio::time::timeout(BUDGET, buscar())
        .await
        .ok()
        .flatten();
    // Timestamp gravado em sucesso OU falha de rede — não martelar.
    gravar_last_check(&path, agora_secs());
    let Some(body) = corpo else { return };
    let Some(tag) = extrair_tag(&body) else { return };
    let local = env!("CARGO_PKG_VERSION");
    if version_is_newer(&tag, local) {
        eprintln!("{}", linha_aviso(&tag, local));
    }
}

// ---------- testes ----------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_newer_casos_basicos() {
        // Igual → false (não avisa pela própria versão).
        assert!(!version_is_newer("v0.3.0", "0.3.0"));
        assert!(!version_is_newer("0.3.0", "v0.3.0"));
        // Patch: acima avisa, abaixo não.
        assert!(version_is_newer("v0.3.1", "0.3.0"));
        assert!(!version_is_newer("v0.3.0", "0.3.1"));
        // Minor e Major.
        assert!(version_is_newer("v0.4.0", "0.3.9"));
        assert!(version_is_newer("v1.0.0", "0.99.99"));
        assert!(!version_is_newer("v0.2.9", "0.3.0"));
        // Antiga nunca avisa, mesmo com prefixo sujo.
        assert!(!version_is_newer("v0.1.0", "0.3.0"));
    }

    #[test]
    fn version_is_newer_tolerante_a_sujeira() {
        // Prefixo v/V nos dois lados.
        assert!(version_is_newer("V1.2.3", "v1.2.2"));
        // Sufixo de prerelease/HTML: o número base decide (1.3.0 > 1.2.9).
        assert!(version_is_newer("v1.3.0-rc.1", "1.2.9"));
        assert!(version_is_newer("1.3.0 (Latest)", "1.2.9"));
        // Componentes a menos → tratado como .0 (1.3 > 1.2.9).
        assert!(version_is_newer("1.3", "1.2.9"));
        assert!(!version_is_newer("1.2", "1.2.0"));
        // Lixo → None → false (nunca falso positivo).
        assert!(!version_is_newer("", "0.3.0"));
        assert!(!version_is_newer("v", "0.3.0"));
        assert!(!version_is_newer("abc", "0.3.0"));
        assert!(!version_is_newer("v1.x.0", "0.3.0"));
        // Local malformado → false mesmo com remote maior (não avisa errado).
        assert!(!version_is_newer("v9.9.9", "dev"));
        assert!(!version_is_newer("9.9.9", ""));
    }

    #[test]
    fn deve_checar_janela_de_24h() {
        const DIA: i64 = INTERVALO_SECS;
        // Nunca checou → chega.
        assert!(deve_checar(1_000_000, None));
        // Fresco: 1h e 23h59min atrás → NÃO checa.
        assert!(!deve_checar(1_000_000, Some(1_000_000 - 3_600)));
        assert!(!deve_checar(1_000_000, Some(1_000_000 - DIA + 60)));
        // Exatamente 24h (e um pouco mais) → checa de novo.
        assert!(deve_checar(1_000_000, Some(1_000_000 - DIA)));
        assert!(deve_checar(1_000_000, Some(1_000_000 - DIA - 1)));
        // Relógio atrasado (last no futuro) → não checa (saturação, sem martelar).
        assert!(!deve_checar(1_000_000, Some(1_000_000 + 100)));
    }

    #[test]
    fn extrair_tag_do_corpo_da_api() {
        let corpo = r#"{"url":"https://api.github.com/...","tag_name":"v0.4.0","name":"v0.4.0"}"#;
        assert_eq!(extrair_tag(corpo).as_deref(), Some("v0.4.0"));
        // Sem o campo → None; corpo ilegível → None.
        assert_eq!(extrair_tag(r#"{"name":"x"}"#), None);
        assert_eq!(extrair_tag("<html>rate limit</html>"), None);
        assert_eq!(extrair_tag(""), None);
    }

    #[test]
    fn linha_aviso_formato_e_normalizacao() {
        let l = linha_aviso("v0.4.0", "0.3.0");
        assert!(l.starts_with("nova versão disponível: v0.4.0"), "{l}");
        assert!(l.contains("(instalada: v0.3.0)"), "{l}");
        assert!(
            l.ends_with("cargo install --git https://github.com/danjour/CLI-ZCode"),
            "{l}"
        );
        // Tag sem o prefixo v → normalizada para v.
        assert!(linha_aviso("0.4.0", "0.3.0").contains(": v0.4.0 "));
    }

    #[test]
    fn registro_roundtrip_e_leitura_tolerante() {
        let dir = std::env::temp_dir().join(format!("zc-updchk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("update-check.json");
        // Ausente/ilegível → None.
        assert_eq!(ler_last_check(&path), None);
        std::fs::write(&path, "{lixo").unwrap();
        assert_eq!(ler_last_check(&path), None);
        // Roundtrip via gravar_last_check (que cria o dir se faltar).
        let aninhado = dir.join("sub").join("update-check.json");
        gravar_last_check(&aninhado, 12345);
        assert_eq!(ler_last_check(&aninhado), Some(12345));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
