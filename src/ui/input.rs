//! Linha/multilinha simples (Enter envia; base p/ editor Fase 4).
//!
//! Fase 3: slash commands de controle (stop/mode/model/thought/compact/
//! usage/resume/new/fork/goal). Linha iniciada em `/` nunca vira prompt
//! (evita turno LLM por typo).

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Empty,
    Exit,
    Usage,
    Stop,
    Mode(Option<String>),
    Model(Option<String>),
    Thought(Option<String>),
    Compact,
    Resume(Option<String>),
    New(Option<String>),
    Fork(Option<String>),
    Goal(Option<String>),
    /// `/help` — lista comandos e atalhos (local, sem RPC).
    Help,
    /// `/diff` — git diff --stat + diff do snapshot do último turno.
    Diff,
    /// `/` desconhecido — erro claro, sem enviar turno.
    Unknown(String),
    Text(String),
}

fn arg(rest: &str) -> Option<String> {
    let t = rest.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.split_whitespace().next().unwrap_or("").to_string())
    }
}

/// Classifica uma linha do REPL. Vazio (só espaces) → Empty (volta ao prompt).
pub fn parse_input(line: &str) -> Input {
    let t = line.trim();
    if t.is_empty() {
        return Input::Empty;
    }
    let (cmd, rest) = match t.split_once(char::is_whitespace) {
        Some((c, r)) => (c, r),
        None => (t, ""),
    };
    match cmd {
        "/exit" | "/quit" => Input::Exit,
        "/usage" => Input::Usage,
        "/stop" => Input::Stop,
        "/mode" => Input::Mode(arg(rest)),
        "/model" => Input::Model(arg(rest)),
        "/thought" => Input::Thought(arg(rest)),
        "/compact" => Input::Compact,
        "/resume" => Input::Resume(arg(rest)),
        "/new" => Input::New(arg(rest)),
        "/fork" => Input::Fork(arg(rest)),
        "/goal" => Input::Goal(arg(rest)),
        "/help" | "/?" | "/ajuda" => Input::Help,
        "/diff" => Input::Diff,
        c if c.starts_with('/') => Input::Unknown(c.to_string()),
        _ => Input::Text(t.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_vazio_volta_ao_prompt() {
        assert_eq!(parse_input(""), Input::Empty);
        assert_eq!(parse_input("   "), Input::Empty);
    }

    #[test]
    fn comandos() {
        assert_eq!(parse_input("/exit"), Input::Exit);
        assert_eq!(parse_input("/usage"), Input::Usage);
        assert_eq!(
            parse_input("  oi  "),
            Input::Text("oi".to_string())
        );
    }

    #[test]
    fn fase3_todos_parseados_fixtures() {
        // Zero gasto live: só classificação de strings.
        assert_eq!(parse_input("/stop"), Input::Stop);
        assert_eq!(parse_input("/mode plan"), Input::Mode(Some("plan".into())));
        assert_eq!(parse_input("/mode"), Input::Mode(None));
        assert_eq!(
            parse_input("/model zai/glm-5.3-Flash"),
            Input::Model(Some("zai/glm-5.3-Flash".into()))
        );
        assert_eq!(parse_input("/model"), Input::Model(None));
        assert_eq!(
            parse_input("/thought max"),
            Input::Thought(Some("max".into()))
        );
        assert_eq!(parse_input("/thought"), Input::Thought(None));
        assert_eq!(parse_input("/compact"), Input::Compact);
        assert_eq!(parse_input("/resume sess_1"), Input::Resume(Some("sess_1".into())));
        assert_eq!(parse_input("/resume"), Input::Resume(None));
        assert_eq!(parse_input("/new C:/p"), Input::New(Some("C:/p".into())));
        assert_eq!(parse_input("/new"), Input::New(None));
        assert_eq!(parse_input("/fork sess_1"), Input::Fork(Some("sess_1".into())));
        assert_eq!(parse_input("/goal show"), Input::Goal(Some("show".into())));
        assert_eq!(parse_input("/goal"), Input::Goal(None));
        // Fase 8 (TUI visual): /help e /diff entram no parser.
        assert_eq!(parse_input("/help"), Input::Help);
        assert_eq!(parse_input("/?"), Input::Help);
        assert_eq!(parse_input("/ajuda"), Input::Help);
        assert_eq!(parse_input("/diff"), Input::Diff);
        assert_eq!(parse_input("/diff extra"), Input::Diff);
    }

    #[test]
    fn slash_desconhecido_nao_vira_turno() {
        assert_eq!(parse_input("/modle"), Input::Unknown("/modle".into()));
        assert_eq!(parse_input("/foo bar"), Input::Unknown("/foo".into()));
    }
}
