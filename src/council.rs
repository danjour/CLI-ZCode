//! Conselho multi-agente: N workers + 1 chefe discutem uma pergunta.
//!
//! Não usa `session/subagents` nativo (launch é tool interno do runtime —
//! ver `docs/PROTOCOLO-SUBAGENTS.md`). Orquestra sessões top-level via o
//! mesmo `Transport` do CLI, com memória de trabalho em arquivo por agente.

use crate::commands::CmdError;
use crate::daemon_client::Transport;
use crate::session;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

/// Papéis default (até `agents`; se >3, extras viram generalistas numerados).
pub fn default_roles(n: u8) -> Vec<String> {
    let base = [
        "Implementação (como fazer, ordem de passos, custo)",
        "Riscos & falhas (o que pode quebrar, edge cases, segurança)",
        "Alternativas (outros caminhos, trade-offs, o que não fazer)",
    ];
    let n = n.clamp(2, 8) as usize;
    (0..n)
        .map(|i| {
            base.get(i)
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("Especialista {} (ângulo extra da pergunta)", i + 1))
        })
        .collect()
}

/// Pasta de memória do conselho (`%APPDATA%/zcode-cli/council/<id>`).
pub fn council_dir(run_id: &str) -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("zcode-cli")
        .join("council")
        .join(run_id)
}

pub fn agent_memory_path(dir: &PathBuf, agent_idx: usize) -> PathBuf {
    dir.join(format!("agent-{agent_idx}.md"))
}

pub fn chair_path(dir: &PathBuf) -> PathBuf {
    dir.join("chair.md")
}

pub fn board_path(dir: &PathBuf) -> PathBuf {
    dir.join("board.md")
}

/// Prompt da rodada 1 (resposta inicial do worker).
pub fn prompt_round1(question: &str, role: &str, memory: &str) -> String {
    let mem = if memory.trim().is_empty() {
        "(sem memória anterior)".to_string()
    } else {
        format!("## Sua memória de rodadas anteriores\n{memory}")
    };
    format!(
        "Você é um membro do conselho. Seu papel nesta discussão:\n\
         **{role}**\n\n\
         ## Pergunta do conselho\n{question}\n\n\
         {mem}\n\n\
         Responda APENAS do seu papel. Máximo ~400 palavras. \
         Seja concreto (passos, números, nomes). \
         Termine com 2–3 bullets em `## Pontos-chave`."
    )
}

/// Prompt de crítica (rodada 2+): lê o board e ataca/refina.
pub fn prompt_critique(question: &str, role: &str, board: &str, memory: &str, round: u8) -> String {
    let mem = if memory.trim().is_empty() {
        String::new()
    } else {
        format!("## Sua memória\n{memory}\n\n")
    };
    format!(
        "Você é um membro do conselho. Papel:\n**{role}**\n\n\
         ## Pergunta\n{question}\n\n\
         ## Quadro atual (respostas dos pares, rodada {})\n{board}\n\n\
         {mem}\
         Crítica construtiva: aponte falhas lógicas, o que falta, onde os \
         outros estão certos/errados. Máximo ~300 palavras. \
         Se concordar, diga o que preservar. \
         Termine com `## Revisão` (sua posição atual em 3 bullets).",
        round
    )
}

/// Prompt do chefe (síntese final).
pub fn prompt_chair(question: &str, board: &str) -> String {
    format!(
        "Você é o CHEFE do conselho. Não é um worker.\n\n\
         ## Pergunta\n{question}\n\n\
         ## Debate completo\n{board}\n\n\
         Produza a decisão do conselho:\n\
         1. **Consenso** — o que todos (ou maioria) aceitam\n\
         2. **Divergências** — pontos em aberto e quem defendeu o quê\n\
         3. **Recomendação** — ação concreta (faça X, evite Y, meça Z)\n\
         4. **Próximos passos** — 3 itens no máximo\n\n\
         Máximo ~500 palavras. Sem floreio. Se não houver consenso, diga."
    )
}

/// Junta as respostas num board markdown.
pub fn build_board(question: &str, rounds: &[(u8, Vec<(String, String)>)]) -> String {
    let mut out = format!("# Conselho — {question}\n");
    for (round, answers) in rounds {
        out.push_str(&format!("\n## Rodada {round}\n"));
        for (who, text) in answers {
            out.push_str(&format!("\n### {who}\n\n{text}\n"));
        }
    }
    out
}

/// Anexa trecho à memória do agente (append, com cabeçalho de rodada).
pub fn append_memory(path: &PathBuf, round: u8, text: &str) {
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let mut prev = std::fs::read_to_string(path).unwrap_or_default();
    if !prev.is_empty() && !prev.ends_with('\n') {
        prev.push('\n');
    }
    prev.push_str(&format!("\n---\n\n## Rodada {round}\n\n{text}\n"));
    let _ = std::fs::write(path, prev);
}

fn read_memory(path: &PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Valida limites do conselho (puro, testável).
pub fn validate_council(agents: u8, rounds: u8) -> Result<(), String> {
    if !(2..=8).contains(&agents) {
        return Err(format!("--agents deve ser 2–8 (recebido {agents})"));
    }
    if !(1..=4).contains(&rounds) {
        return Err(format!("--rounds deve ser 1–4 (recebido {rounds})"));
    }
    Ok(())
}

/// Executa o conselho. `rt` aberto no workspace `ws`.
pub async fn run_council(
    rt: &Arc<Transport>,
    ws: &str,
    question: &str,
    agents: u8,
    rounds: u8,
    timeout_secs: u64,
    json: bool,
) -> Result<(), CmdError> {
    validate_council(agents, rounds).map_err(CmdError::Session)?;
    let n = agents.clamp(2, 8) as usize;
    let rounds = rounds.clamp(1, 4) as u8;
    let roles = default_roles(agents);

    let run_id = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let dir = council_dir(&run_id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| CmdError::Io(format!("criar {}: {e}", dir.to_string_lossy())))?;

    if !json {
        println!("conselho {run_id}: {n} agentes × {rounds} rodada(s)");
        println!("memória: {}", dir.to_string_lossy());
        println!("pergunta: {question}");
    }

    // Sessões dos workers (uma por agente). Modo `plan`: discussão sem
    // ferramentas que exigam approve (headless `build` trava em permission).
    let mut worker_sids = Vec::with_capacity(n);
    for _ in 0..n {
        let created = session::create_session(rt, ws).await?;
        let sid = session::extract_session_id(&created)
            .ok_or_else(|| CmdError::Session("create worker não devolveu sessionId".into()))?;
        let _ = session::apply_mode(rt, &sid, "plan").await;
        worker_sids.push(sid);
    }

    let mut history: Vec<(u8, Vec<(String, String)>)> = Vec::new();

    for round in 1..=rounds {
        let mut answers = Vec::with_capacity(n);
        for (i, sid) in worker_sids.iter().enumerate() {
            let role = &roles[i];
            let mem_path = agent_memory_path(&dir, i + 1);
            let mem = read_memory(&mem_path);
            let prompt = if round == 1 {
                prompt_round1(question, role, &mem)
            } else {
                let board = build_board(question, &history);
                prompt_critique(question, role, &board, &mem, round)
            };
            if !json {
                println!("  rodada {round} · agente {} ({}) …", i + 1, short_role(role));
            }
            match session::send_and_wait(rt, sid, &prompt, timeout_secs).await {
                Ok((_raw, text)) => {
                    append_memory(&mem_path, round, &text);
                    answers.push((format!("Agente {} — {}", i + 1, short_role(role)), text));
                }
                Err(e) => {
                    let msg = format!("(falhou: {e})");
                    answers.push((format!("Agente {} — {}", i + 1, short_role(role)), msg));
                }
            }
        }
        history.push((round, answers));
    }

    // Síntese do chefe em sessão própria.
    let board = build_board(question, &history);
    let _ = std::fs::write(board_path(&dir), &board);
    let chair_created = session::create_session(rt, ws).await?;
    let chair_sid = session::extract_session_id(&chair_created)
        .ok_or_else(|| CmdError::Session("create chefe não devolveu sessionId".into()))?;
    let _ = session::apply_mode(rt, &chair_sid, "plan").await;
    if !json {
        println!("  chefe sintetiza …");
    }
    let chair_prompt = prompt_chair(question, &board);
    let chair_text = match session::send_and_wait(rt, &chair_sid, &chair_prompt, timeout_secs).await
    {
        Ok((_raw, text)) => text,
        Err(e) => format!("(síntese falhou: {e})"),
    };
    let _ = std::fs::write(chair_path(&dir), &chair_text);

    if json {
        let rounds_json: Vec<Value> = history
            .iter()
            .map(|(r, answers)| {
                let items: Vec<Value> = answers
                    .iter()
                    .map(|(who, text)| serde_json::json!({"who": who, "text": text}))
                    .collect();
                serde_json::json!({"round": r, "answers": items})
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "runId": run_id,
                "question": question,
                "agents": n,
                "rounds": rounds,
                "memoryDir": dir.to_string_lossy(),
                "board": board,
                "synthesis": chair_text,
                "roundsDetail": rounds_json,
                "sessionIds": {
                    "workers": worker_sids,
                    "chair": chair_sid,
                },
            }))
            .unwrap_or_else(|_| "{}".into())
        );
    } else {
        println!("\n========== SÍNTESE DO CHEFE ==========\n");
        println!("{chair_text}");
        println!("\nmemória em {}", dir.to_string_lossy());
    }
    Ok(())
}

fn short_role(role: &str) -> String {
    role.split(" (")
        .next()
        .unwrap_or(role)
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_default_2_a_8() {
        let r2 = default_roles(2);
        assert_eq!(r2.len(), 2);
        assert!(r2[0].contains("Implementação"));
        let r5 = default_roles(5);
        assert_eq!(r5.len(), 5);
        assert!(r5[3].contains("Especialista 4"));
        // 0 e 1 são clampados para o mínimo/máximo honestos no run_council;
        // default_roles aceita o valor e clampa internamente.
        assert_eq!(default_roles(0).len(), 2);
        assert_eq!(default_roles(20).len(), 8);
    }

    #[test]
    fn validate_limites() {
        assert!(validate_council(3, 2).is_ok());
        assert!(validate_council(1, 2).is_err());
        assert!(validate_council(9, 2).is_err());
        assert!(validate_council(3, 0).is_err());
        assert!(validate_council(3, 5).is_err());
    }

    #[test]
    fn prompts_contem_papel_e_pergunta() {
        let p1 = prompt_round1("migrar para X?", "Riscos", "");
        assert!(p1.contains("migrar para X?"));
        assert!(p1.contains("Riscos"));
        assert!(p1.contains("Pontos-chave"));
        let p2 = prompt_critique("Q", "Alt", "board-aqui", "mem", 2);
        assert!(p2.contains("board-aqui"));
        assert!(p2.contains("Revisão"));
        let pc = prompt_chair("Q", "debate");
        assert!(pc.contains("Consenso"));
        assert!(pc.contains("Recomendação"));
    }

    #[test]
    fn board_e_memoria_roundtrip() {
        let rounds = vec![
            (1u8, vec![("Agente 1 — Impl".into(), "resp a".into())]),
            (2u8, vec![("Agente 1 — Impl".into(), "resp b".into())]),
        ];
        let b = build_board("Q?", &rounds);
        assert!(b.contains("Rodada 1"));
        assert!(b.contains("resp a"));
        assert!(b.contains("Rodada 2"));
        let dir = std::env::temp_dir().join(format!("zc-council-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mp = agent_memory_path(&dir, 1);
        append_memory(&mp, 1, "linha1");
        append_memory(&mp, 2, "linha2");
        let m = std::fs::read_to_string(&mp).unwrap();
        assert!(m.contains("linha1") && m.contains("linha2"));
        assert!(m.contains("Rodada 1") && m.contains("Rodada 2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_role_encurta() {
        assert_eq!(
            short_role("Implementação (como fazer, ordem de passos, custo)"),
            "Implementação"
        );
    }
}
