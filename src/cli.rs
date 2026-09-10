//! CLI headless (flags + subcomandos Fase 1+2).

use clap::{Parser, Subcommand};

#[derive(Debug, Clone, Parser)]
#[command(name = "zcode-cli", version, about = "CLI Rust para o runtime oficial ZCode — TUI, REPL e headless")]
pub struct Cli {
    /// Workspace do projeto (aceita C:/ e C:\).
    #[arg(long, global = true)]
    pub cwd: Option<String>,

    /// Modelo providerId/modelId (ex.: zai/glm-5.3).
    #[arg(long, global = true)]
    pub model: Option<String>,

    /// Modo de permissão.
    #[arg(long, global = true, value_parser = ["plan", "build", "edit", "yolo"])]
    pub mode: Option<String>,

    /// Nível de raciocínio.
    #[arg(long, global = true, value_parser = ["low", "high", "max"])]
    pub thought_level: Option<String>,

    /// Saída machine-readable (JSON).
    #[arg(long, global = true, default_value_t = false)]
    pub json: bool,

    /// One-shot: envia o prompt e sai.
    #[arg(short = 'p', long, global = true)]
    pub prompt: Option<String>,

    /// Continua a última sessão da pasta.
    #[arg(short = 'c', long = "continue", global = true, default_value_t = false)]
    pub cont: bool,

    /// Override do caminho do zcode.cjs (também via env ZCODE_CJS).
    #[arg(long, global = true)]
    pub runtime: Option<String>,

    /// Allowlist de ferramentas (ex.: "Bash(git *)"). Parse client-side;
    /// sem setter RPC verificado → aviso honesto de não-aplicado (Fase 5).
    #[arg(long = "allowed-tools", global = true)]
    pub allowed_tools: Option<String>,

    /// Denylist de ferramentas (ex.: "Edit"). Idem --allowed-tools.
    #[arg(long = "disallowed-tools", global = true)]
    pub disallowed_tools: Option<String>,

    /// Ao concluir o one-shot, notifica (beep; ou comando de --notify-cmd).
    #[arg(long = "notify-on-done", global = true, default_value_t = false)]
    pub notify_on_done: bool,

    /// Comando executado quando --notify-on-done (ex.: "notify-send pronto").
    #[arg(long = "notify-cmd", global = true)]
    pub notify_cmd: Option<String>,

    /// Nunca usa nem auto-inicia o daemon: runtime embutido por comando
    /// (comportamento pré-Fase 3; o fallback automático é este mesmo).
    #[arg(long = "no-daemon", global = true, default_value_t = false)]
    pub no_daemon: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Commands {
    /// Lista sessões (session/list).
    Sessions {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Continua uma sessão (session/resume). Sem id + -c = última da pasta.
    Resume {
        id: Option<String>,
    },
    /// Cria sessão num workspace (session/create).
    New {
        pasta: String,
    },
    /// Fork de sessão (session/fork).
    Fork {
        id: String,
    },
    /// Uso de tokens (session/usage).
    Usage {
        id: Option<String>,
    },
    /// Diagnóstico local sem gastar plano (node, zcode.cjs, TOML, modelos).
    Doctor,
    /// Daemon/broker residente (Fase 3): processo foreground dono dos runtimes
    /// Node — sessões ficam quentes entre comandos. O cliente padrão usa o
    /// daemon quando `daemon.json` existe e responde (auto-start por comando;
    /// `--no-daemon` força runtime embutido). Log no mesmo tracing do CLI.
    Daemon {
        /// Lê daemon.json, conecta (token) e pede shutdown ao daemon em
        /// execução (parada limpa: kill_tree dos filhos + remove daemon.json).
        #[arg(long, default_value_t = false)]
        stop: bool,
    },
}
