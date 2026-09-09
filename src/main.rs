//! zcode-cli — entrada: parse args, tracing em arquivo, dispatch headless.
//! TUI (Maria) consome o mesmo runtime depois; aqui só headless (Fase 1+2).

mod cli;
mod commands;
mod config;
mod daemon;
mod daemon_client;
mod doctor;
mod rpc;
mod runtime;
mod server_requests;
mod session;
mod ui;

use clap::Parser;
use std::fs::OpenOptions;
use std::sync::{Arc, Mutex};

/// Pasta de dados do app (`%APPDATA%/zcode-cli` no Windows via
/// `dirs::data_dir`), com fallback p/ a pasta corrente quando o dirs falha.
fn data_dir() -> std::path::PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("zcode-cli")
}

/// Caminho do log do tracing — o MESMO arquivo recebe o relatório de pânico
/// em append (compartilhado com o panic hook da TUI em commands.rs).
pub(crate) fn log_file_path() -> std::path::PathBuf {
    data_dir().join("log.txt")
}

/// Teto do log antes de rodar: 1 MiB (2^20 bytes). Acima disso o arquivo é
/// rotacionado no startup — `log.txt` não cresce sem limites.
pub(crate) const LOG_ROTATE_MAX_BYTES: u64 = 1 << 20;

/// Decisão pura da rotação (testável): o tamanho passou do teto? (`>` — no
/// teto exato ainda não roda, só na 1ª escrita depois dele).
pub(crate) fn should_rotate_log(size: u64) -> bool {
    size > LOG_ROTATE_MAX_BYTES
}

/// Rotação best-effort: `log.txt` → `log.txt.old`, substituindo o `.old`
/// anterior (fica 1 geração de histórico). Erros são engolidos — log nunca
/// impede o app de rodar.
fn rotate_log(log_path: &std::path::Path) {
    let Some(nome) = log_path.file_name() else {
        return;
    };
    let old = log_path.with_file_name(format!("{}.old", nome.to_string_lossy()));
    let _ = std::fs::remove_file(&old); // geração anterior é descartada
    let _ = std::fs::rename(log_path, old);
}

fn setup_tracing() {
    let dir = data_dir();
    // Log em ~/.local/share/zcode-cli/log.txt no Windows (dirs::data_dir),
    // com fallback p/ ~/.local/state/zcode-cli/log.txt no Linux.
    let _ = std::fs::create_dir_all(&dir);
    let log_path = dir.join("log.txt");
    // Rotação com teto ANTES de abrir: no Windows, renomear um arquivo já
    // aberto pelo tracing falha (sharing violation). Best-effort.
    if let Ok(md) = std::fs::metadata(&log_path) {
        if should_rotate_log(md.len()) {
            rotate_log(&log_path);
        }
    }
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(&log_path) {
        let writer = Arc::new(Mutex::new(f));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("zcode_cli=debug")),
            )
            .with_writer(move || TracingFileWriter(writer.clone()))
            .try_init();
    } else {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("zcode_cli=info")),
            )
            .try_init();
    }
}

struct TracingFileWriter(Arc<Mutex<std::fs::File>>);
impl std::io::Write for TracingFileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().map_err(|_| std::io::Error::other("lock"))?.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().map_err(|_| std::io::Error::other("lock"))?.flush()
    }
}

/// Decisão `--tui` × `-p/--prompt`: a TUI não executa one-shot. Com ambos,
/// retorna o aviso (Err) e o prompt é ignorado — sem mudar exit code nem o
/// resto do comportamento.
fn tui_com_prompt(tui: bool, prompt: Option<&str>) -> Result<Option<&str>, String> {
    if tui {
        return match prompt {
            Some(_) => Err("aviso: --tui não executa -p/--prompt; o prompt foi ignorado".into()),
            None => Ok(None),
        };
    }
    Ok(prompt)
}

#[tokio::main]
async fn main() {
    setup_tracing();
    // Modo TUI (Fase 4) sem tocar cli.rs: `zcode-cli tui` ou `--tui`.
    // Só argv[1] == "tui" conta como subcomando (`new tui` continua pasta).
    let raw: Vec<String> = std::env::args().collect();
    let tui_mode = raw.iter().any(|s| s == "--tui")
        || raw.get(1).map(|s| s == "tui").unwrap_or(false);
    let mut filtered = Vec::with_capacity(raw.len());
    for (i, a) in raw.into_iter().enumerate() {
        if a == "--tui" || (i == 1 && a == "tui") {
            continue;
        }
        filtered.push(a);
    }
    let cli = match cli::Cli::try_parse_from(filtered) {
        Ok(c) => c,
        Err(e) => e.exit(),
    };
    // `--tui` não executa `-p/--prompt`: avisa em stderr e segue com a TUI.
    if let Err(aviso) = tui_com_prompt(tui_mode, cli.prompt.as_deref()) {
        eprintln!("{aviso}");
    }
    // Exit codes Fase 5: 0 sucesso · 1 erro · 2 turno parado.
    let res = if tui_mode {
        commands::run_tui(cli).await
    } else {
        commands::run(cli).await
    };
    if let Err(e) = res {
        tracing::error!(%e, "falha");
        eprintln!("erro: {e}");
        std::process::exit(commands::exit_code(&e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_com_prompt_avisa_so_quando_ignora() {
        // TUI sem prompt: silenciosa.
        assert_eq!(tui_com_prompt(true, None), Ok(None));
        // TUI com prompt: aviso claro citando as duas flags.
        let aviso = tui_com_prompt(true, Some("oi")).unwrap_err();
        assert!(aviso.contains("--tui"), "{aviso}");
        assert!(aviso.contains("--prompt"), "{aviso}");
        // Headless: prompt intacto (com e sem).
        assert_eq!(tui_com_prompt(false, Some("oi")), Ok(Some("oi")));
        assert_eq!(tui_com_prompt(false, None), Ok(None));
    }

    #[test]
    fn should_rotate_log_teto_1mib() {
        assert!(!should_rotate_log(0), "log novo nunca roda");
        assert!(
            !should_rotate_log(LOG_ROTATE_MAX_BYTES),
            "exatamente no teto ainda não roda"
        );
        assert!(should_rotate_log(LOG_ROTATE_MAX_BYTES + 1), "1 byte acima roda");
        assert!(should_rotate_log(u64::MAX), "saturação sem overflow");
    }

    #[test]
    fn rotacao_log_renomeia_e_substitui_old() {
        // I/O real em temp dir: o caminho feliz da rotação é verificável (as
        // falhas seguem best-effort — silenciosas — no código real).
        let dir = std::env::temp_dir().join(format!("zc-logrot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log.txt");
        std::fs::write(&log, "conteudo antigo").unwrap();
        rotate_log(&log);
        assert!(!log.exists(), "log.txt sai do lugar (vira .old)");
        assert_eq!(
            std::fs::read_to_string(dir.join("log.txt.old")).unwrap(),
            "conteudo antigo"
        );
        // 2ª rotação SUBSTITUI o .old anterior (uma única geração de backup).
        std::fs::write(&log, "conteudo novo").unwrap();
        rotate_log(&log);
        assert_eq!(
            std::fs::read_to_string(dir.join("log.txt.old")).unwrap(),
            "conteudo novo"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
