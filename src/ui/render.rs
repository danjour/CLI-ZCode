//! Layout texto (base p/ ratatui Fase 4). Sem dependência nova.

use crate::session::{AvailableModel, TodoItem, TodoStatus};
use serde_json::Value;

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>())
    }
}

/// Tabela `sessions` (id, título, modo, status, data) — consome `session/list`.
pub fn format_sessions_table(v: &Value) -> String {
    let arr = v
        .get("sessions")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    if arr.is_empty() {
        return "(nenhuma sessão)".to_string();
    }
    let mut out = format!(
        "{:<24} {:<30} {:<8} {:<10} {}\n",
        "id", "titulo", "modo", "status", "data"
    );
    for s in &arr {
        let id = s.get("sessionId").and_then(|x| x.as_str()).unwrap_or("-");
        let title = s.get("title").and_then(|x| x.as_str()).unwrap_or("-");
        let mode = s.get("mode").and_then(|x| x.as_str()).unwrap_or("-");
        let status = s.get("status").and_then(|x| x.as_str()).unwrap_or("-");
        let created = s.get("createdAt").and_then(|x| x.as_str()).unwrap_or("-");
        out.push_str(&format!(
            "{:<24} {:<30} {:<8} {:<10} {}\n",
            trunc(id, 24),
            trunc(title, 30),
            mode,
            status,
            created
        ));
    }
    out
}

/// Fallback: histórico local quando `session/list` volta vazio.
pub fn format_local_history(entries: &[(String, String, String)]) -> String {
    if entries.is_empty() {
        return "(nenhuma sessão)".to_string();
    }
    let mut out = "(servidor vazio; histórico local)\n".to_string();
    for (id, title, ws) in entries {
        out.push_str(&format!("{} | {} | {}\n", trunc(id, 24), trunc(title, 30), ws));
    }
    out
}

pub fn format_message(role: &str, text: &str) -> String {
    match role {
        "assistant" => text.to_string(),
        "user" => format!("você: {text}"),
        _ => format!("[{role}] {text}"),
    }
}

pub fn welcome(session_id: &str) -> String {
    format!(
        "sessão {session_id} — /help /stop /mode /model /thought /compact /usage /goal /exit.\n\
         /resume /new /fork: leitura e gestão (R1; fork exige checkpoint R4)."
    )
}

/// Compat de call site (commands.rs): sem wordmark nem arte de branding —
/// retorna o welcome comum. Mantida a assinatura até o wiring do Codex.
pub fn welcome_retro(session_id: &str) -> String {
    welcome(session_id)
}

/// Splash de abertura: injeta a mensagem-MARCADOR no histórico da TUI (mesmo
/// role "system" de antes). O conteúdo visual — arte half-block truecolor —
/// é resolvido pelo renderer (`ui::art::art_text`); REPL/headless usam
/// `ui::art::splash_stdout`. A assinatura mantém `width` por compatibilidade
/// dos call sites.
pub fn splash(_width: u16) -> &'static str {
    crate::ui::art::SPLASH_MARKER
}

/// Aviso R1: resume é leitura nesta rodada.
pub fn resume_readonly_notice() -> String {
    "sessão retomada em modo leitura (R1: novo turno só em sessão warm ou nova). P/ continuar, use: new <pasta> ou zcode-cli sem args.".to_string()
}

/// Lista `settings.model.available` (sem nova chamada; R3: só Flash ao vivo).
pub fn format_models_list(models: &[AvailableModel]) -> String {
    if models.is_empty() {
        return "(nenhum modelo listado pelo servidor)".to_string();
    }
    let mut out = "modelos disponíveis (uso: /model <providerId/modelId>):\n".to_string();
    for m in models {
        out.push_str(&format!(
            "- {} ({} — ctx {})\n",
            m.label,
            format!("{}/{}", m.provider_id, m.model_id),
            m.context_window
        ));
    }
    out
}

/// Confirmação OBRIGATÓRIA do /compact (dispara sumarização LLM: gasta plano).
pub fn compact_confirm_prompt() -> String {
    "⚠ /compact dispara sumarização via LLM e GASTA PLANO. Confirmar? [y/n]".to_string()
}

/// fork sem checkpoint do servidor (R4).
pub fn fork_no_checkpoint_hint() -> String {
    "fork falhou (R4: exige workspace checkpoint do servidor; fork conversacional não existe).".to_string()
}

/// goal set/replace bloqueados (campo de texto não verificado — sem invenção).
pub fn goal_write_blocked_hint() -> String {
    "goal set/replace indisponíveis: campo de texto ainda não verificado no servidor (use show|pause|resume|clear).".to_string()
}

/// Permissões (Tempo 2, contrato-fase4-5 §2): sem método RPC cliente→servidor
/// p/ aprovar/negar nesta versão — componente oculto; use --mode yolo (R2).
pub fn perm_unsupported_note() -> String {
    "permissões: sem broker RPC nesta versão (approve/deny não-suportado) — o prompt y/n não aprova nada server-side; use --mode yolo p/ ferramentas executarem (R2).".to_string()
}

/// Texto do /help: comandos + atalhos (100% local, sem RPC).
pub fn help_text() -> String {
    "comandos: /exit /usage /context /todos /stop /mode /model /thought /compact /resume /new /fork /goal /diff /export /help\n\
     atalhos: Enter envia · Ctrl+J nova linha · Ctrl+F busca no transcript (Enter salta e fecha; ↑/↓ troca; F3/Shift+F3 próximo/anterior; Esc cancela) · Esc para o turno · Ctrl+N nova sessão · Ctrl+U usage · PageUp/PageDown scroll · roda = scroll · Ctrl+C 2× sai\n\
     /export [caminho] [--json]: salva a conversa em Markdown (default ./zcode-export-{sid8}.md)"
        .to_string()
}

/// `/todos` textual (REPL; base p/ o overlay da TUI): contagem feitos/total +
/// um item por linha com checkbox `[ ]`/`[~]`/`[x]`. Vazio → mensagem honesta
/// (a mesma do overlay, p/ leitura consistente nos dois modos).
pub fn format_todos(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "sem todos registrados (o agente ainda não planejou)".to_string();
    }
    let feitos = todos
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count();
    let mut out = format!("todos ({feitos}/{}):\n", todos.len());
    for t in todos {
        let marca = match t.status {
            TodoStatus::Pending => "[ ]",
            TodoStatus::InProgress => "[~]",
            TodoStatus::Completed => "[x]",
        };
        out.push_str(&format!("{marca} {}\n", t.content));
    }
    out
}

/// Notice curta do Enter bloqueado enquanto a 1ª sessão nasce (startup
/// instantâneo): sem spawn, buffer preservado pelo handler.
pub fn session_pending_notice() -> String {
    "conectando: a sessão ainda está sendo criada — Ctrl+N p/ tentar de novo.".to_string()
}

/// Notice de falha do boot (spawn do runtime ou create da sessão): degradação
/// honesta — sem crash, retry via Ctrl+N.
pub fn session_boot_fail_notice(erro: &str) -> String {
    format!("falha ao criar sessão: {erro} — Ctrl+N para tentar de novo")
}

/// Banner do runtime morto: exit code + últimas ~3 linhas de stderr (o tail
/// chega de `RuntimeError::Exited(code, stderr_tail)`). Linhas vazias são
/// ignoradas; o trecho é truncado p/ não estourar o transcript.
pub fn runtime_dead_banner(exit: Option<i32>, stderr_tail: &str) -> String {
    let code = exit.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
    let lines: Vec<&str> = stderr_tail
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = if lines.is_empty() {
        "(vazio)".to_string()
    } else {
        let start = lines.len().saturating_sub(3);
        trunc(&lines[start..].join(" | "), 160)
    };
    format!("⚠ runtime caiu (exit {code}). stderr: {tail}. Use /exit e reabra.")
}

/// Enter bloqueado com runtime morto: instrução única e clara.
pub fn runtime_dead_blocked() -> String {
    "runtime morto: não há processo p/ enviar turnos. Use /exit (ou Ctrl+C 2×) e reabra o CLI.".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabela_vazia() {
        assert_eq!(
            format_sessions_table(&serde_json::json!({"sessions":[]})),
            "(nenhuma sessão)"
        );
    }

    #[test]
    fn tabela_com_linha() {
        let v = serde_json::json!({"sessions":[{"sessionId":"sess_1","title":"t","mode":"build","status":"active","createdAt":"2026-09-08"}]});
        let t = format_sessions_table(&v);
        assert!(t.contains("sess_1"));
        assert!(t.contains("titulo"));
    }

    #[test]
    fn historico_local_fallback() {
        let t = format_local_history(&[]);
        assert_eq!(t, "(nenhuma sessão)");
        let t2 = format_local_history(&[("sess_a".into(), "minha".into(), "C:/p".into())]);
        assert!(t2.contains("sess_a"));
    }

    #[test]
    fn aviso_r1_menciona_new() {
        assert!(resume_readonly_notice().contains("new"));
    }

    #[test]
    fn welcome_retro_sem_branding() {
        let w = welcome_retro("sess_1");
        assert!(w.contains("sess_1"));
        assert!(!w.contains("ZCODE"));
        assert!(!w.contains("Zcode"));
        assert_eq!(w, welcome("sess_1")); // mesma saída do comum
    }

    #[test]
    fn splash_e_marcador_estavel() {
        // A splash do histórico agora é um marcador estável (a arte half-block
        // é desenhada pelo renderer); independente da largura pedida.
        assert!(!splash(80).is_empty());
        assert_eq!(splash(40), splash(80));
        assert_eq!(splash(40), crate::ui::art::SPLASH_MARKER);
    }

    #[test]
    fn modelos_e_confirms_fixtures() {
        use crate::session::AvailableModel;
        let vazio = format_models_list(&[]);
        assert!(vazio.contains("nenhum modelo"));
        let ms = vec![AvailableModel {
            label: "glm-5.3-Flash".into(),
            provider_id: "zai".into(),
            model_id: "glm-5.3-Flash".into(),
            context_window: 1_000_000,
        }];
        let t = format_models_list(&ms);
        assert!(t.contains("zai/glm-5.3-Flash"));
        assert!(t.contains("/model"));
        assert!(compact_confirm_prompt().contains("GASTA PLANO"));
        assert!(fork_no_checkpoint_hint().contains("R4"));
        assert!(goal_write_blocked_hint().contains("show|pause|resume|clear"));
        assert!(perm_unsupported_note().contains("--mode yolo"));
    }

    #[test]
    fn help_text_lista_comandos_e_atalhos() {
        let h = help_text();
        for cmd in [
            "/exit", "/usage", "/context", "/todos", "/stop", "/mode", "/model", "/thought",
            "/compact", "/resume", "/new", "/fork", "/goal", "/diff", "/export", "/help",
        ] {
            assert!(h.contains(cmd), "falta {cmd}");
        }
        for atalho in ["Enter", "Ctrl+J", "Ctrl+F", "F3", "Esc", "Ctrl+N", "Ctrl+U", "PageUp", "roda", "Ctrl+C"] {
            assert!(h.contains(atalho), "falta atalho {atalho}");
        }
    }

    #[test]
    fn format_todos_contagem_checkboxes_e_vazio() {
        // Vazio: mensagem honesta (idêntica à do overlay da TUI).
        assert_eq!(
            format_todos(&[]),
            "sem todos registrados (o agente ainda não planejou)"
        );
        let todos = vec![
            TodoItem { content: "ler plano".into(), status: TodoStatus::Completed },
            TodoItem { content: "implementar".into(), status: TodoStatus::InProgress },
            TodoItem { content: "testar".into(), status: TodoStatus::Pending },
        ];
        let t = format_todos(&todos);
        assert!(t.starts_with("todos (1/3):"), "{t}");
        assert!(t.contains("[x] ler plano"), "{t}");
        assert!(t.contains("[~] implementar"), "{t}");
        assert!(t.contains("[ ] testar"), "{t}");
        assert_eq!(t.lines().count(), 4, "cabeçalho + 1 por item");
        // 2/3 quando o segundo completa.
        let todos2 = vec![
            TodoItem { content: "a".into(), status: TodoStatus::Completed },
            TodoItem { content: "b".into(), status: TodoStatus::Completed },
            TodoItem { content: "c".into(), status: TodoStatus::Pending },
        ];
        assert!(format_todos(&todos2).starts_with("todos (2/3):"));
    }

    #[test]
    fn notices_de_boot_shape() {
        // Pendente: curta, menciona Ctrl+N, sem assustar.
        let p = session_pending_notice();
        assert!(p.contains("conectando"), "{p}");
        assert!(p.contains("Ctrl+N"), "{p}");
        assert!(p.lines().count() == 1, "uma linha só");
        // Falha: carrega o erro do boot e o caminho de retry.
        let f = session_boot_fail_notice("node não encontrado no PATH");
        assert!(f.contains("falha ao criar sessão"), "{f}");
        assert!(f.contains("node não encontrado no PATH"), "{f}");
        assert!(f.contains("Ctrl+N para tentar de novo"), "{f}");
    }

    #[test]
    fn runtime_dead_banner_ultimas_linhas_e_limites() {
        let tail = "linha1\nlinha2\n\nlinha3\nlinha4\nlinha5\n";
        let b = runtime_dead_banner(Some(1), tail);
        assert!(b.contains("exit 1"), "{b}");
        assert!(b.contains("linha3 | linha4 | linha5"), "últimas 3 linhas: {b}");
        assert!(!b.contains("linha1"), "linhas antigas fora do recorte: {b}");
        assert!(b.contains("/exit e reabra"));
        // Sem exit code conhecido / stderr vazio → degrada sem panica.
        let b2 = runtime_dead_banner(None, "");
        assert!(b2.contains("exit ?"));
        assert!(b2.contains("(vazio)"));
        // Tail gigante é truncado (banner cabe no transcript).
        let longo = "x".repeat(500);
        assert!(runtime_dead_banner(Some(7), &longo).chars().count() < 300);
    }
}
