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
mod update_check;

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

/// Decisão pura do modo TUI (Fase 4) — extraída de `main` p/ testes. MESMA
/// semântica de sempre: `--tui` em QUALQUER posição ativa; só argv[1]=="tui"
/// vale como subcomando (`new tui` continua pasta — "tui" em argv>1 é
/// argumento de outra coisa). Strip espelhado: remove todo `--tui` e o `tui`
/// de argv[1], preservando argv[0] e a ordem do resto. Retorna
/// `(tui_mode, args_filtrados)` — os filtrados seguem direto pro clap.
fn detect_tui_mode(args: &[String]) -> (bool, Vec<String>) {
    let tui_mode =
        args.iter().any(|s| s == "--tui") || args.get(1).map(|s| s == "tui").unwrap_or(false);
    let filtered: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(i, a)| a.as_str() != "--tui" && !(*i == 1 && a.as_str() == "tui"))
        .map(|(_, a)| a.clone())
        .collect();
    (tui_mode, filtered)
}

#[tokio::main]
async fn main() {
    setup_tracing();
    // Modo TUI (Fase 4) sem tocar cli.rs: `zcode-cli tui` ou `--tui`.
    // Decisão/strip extraídos p/ `detect_tui_mode` (testável — semântica
    // documentada lá).
    let raw: Vec<String> = std::env::args().collect();
    let (tui_mode, filtered) = detect_tui_mode(&raw);
    let cli = match cli::Cli::try_parse_from(filtered) {
        Ok(c) => c,
        Err(e) => e.exit(),
    };
    // `--tui` não executa `-p/--prompt`: avisa em stderr e segue com a TUI.
    if let Err(aviso) = tui_com_prompt(tui_mode, cli.prompt.as_deref()) {
        eprintln!("{aviso}");
    }
    // Aviso passivo de nova versão (plano V5-3): task PRÓPRIA, orçamento de
    // rede curto (1,5s) — nunca atrasa nem falha o comando; a saída é UMA
    // linha em STDERR. NÃO roda: no subcomando `daemon` (processo residente,
    // log dedicado), em `--json` (o stdout tem que permanecer limpo p/
    // pipelines) nem na TUI (o terminal é da interface). Erros: silenciosos.
    if !cli.json
        && !matches!(cli.command, Some(cli::Commands::Daemon { .. }))
        && !tui_mode
    {
        tokio::spawn(update_check::run());
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

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detect_tui_mode_tabela() {
        // argv[1]=="tui" é subcomando: mode on + strip do token (argv[0] fica).
        let (m, f) = detect_tui_mode(&v(&["zcode-cli", "tui"]));
        assert!(m);
        assert_eq!(f, v(&["zcode-cli"]));
        // "--tui" em argv[1]: idem (strip espelhado).
        let (m, f) = detect_tui_mode(&v(&["zcode-cli", "--tui"]));
        assert!(m);
        assert_eq!(f, v(&["zcode-cli"]));
        // "tui" depois de argv[1] é argumento de outra coisa: NÃO ativa e
        // NÃO é stripado (`zcode-cli --cwd X tui` não abre a TUI).
        let (m, f) = detect_tui_mode(&v(&["zcode-cli", "--cwd", "X", "tui"]));
        assert!(!m);
        assert_eq!(f, v(&["zcode-cli", "--cwd", "X", "tui"]));
        // `--` não muda nada: "tui" em argv[2] permanece argumento intacto.
        let (m, f) = detect_tui_mode(&v(&["zcode-cli", "--", "tui"]));
        assert!(!m);
        assert_eq!(f, v(&["zcode-cli", "--", "tui"]));
        // Case-sensitive: "TUI" não é subcomando.
        let (m, f) = detect_tui_mode(&v(&["zcode-cli", "TUI"]));
        assert!(!m);
        assert_eq!(f, v(&["zcode-cli", "TUI"]));
        // Subcomando + flag juntos: ambos stripados, mode on (basta 1 marca).
        let (m, f) = detect_tui_mode(&v(&["zcode-cli", "tui", "--tui"]));
        assert!(m);
        assert_eq!(f, v(&["zcode-cli"]));
    }

    #[test]
    fn detect_tui_mode_strip_preserva_ordem_e_casos_de_borda() {
        // Todo `--tui` em qualquer posição sai; o resto mantém a ordem.
        let (m, f) = detect_tui_mode(&v(&["zcode-cli", "-p", "oi", "--tui", "--json"]));
        assert!(m);
        assert_eq!(f, v(&["zcode-cli", "-p", "oi", "--json"]));
        // Sem argv[1]: false, nada a stripar.
        let (m, f) = detect_tui_mode(&v(&["zcode-cli"]));
        assert!(!m);
        assert_eq!(f, v(&["zcode-cli"]));
        // Entrada vazia: degenerada, mas segura.
        let (m, f) = detect_tui_mode(&[]);
        assert!(!m);
        assert!(f.is_empty());
    }

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
