//! Syntax highlight dos fences de código (Fase V5-2) via `syntect`.
//!
//! Decisões (Fase V5-2 do plano):
//! - **Paleta nossa, sem temas HTML**: o syntect NÃO escolhe cores. O bloco é
//!   parseado (`ParseState::parse_line` + replay dos `ScopeStackOp` num
//!   `ScopeStack`) e cada trecho recebe um "slot" semântico (`TokenSlot`)
//!   resolvido por UMA tabela fixa de ~10 seletores scope→slot
//!   (`SCOPE_TABLE`). O mapeamento slot→cor da paleta Dark/Retro/Light vive
//!   no `tui.rs` (quem conhece a `Palette`). Resolução igual à dos temas do
//!   syntect: maior `MatchPower` vence; empate → a entrada mais abaixo na
//!   tabela (mais específica) ganha.
//! - **Fancy-regex, não onig**: backend puro-Rust (`regex-fancy`) — compila
//!   rápido e sem toolchain C; os dumps default do syntect 5 são preparados
//!   para o backend escolhido no build. Se algo falhar (sintaxe
//!   desconhecida, erro de parse, range inválido), o resultado é `None` e o
//!   chamador mantém o bloco CRU atual — o código nunca some nem o
//!   transcript panica.
//! - **1:1 linha→Line ABSOLUTO**: cada linha de `code` gera EXATAMENTE UMA
//!   `Vec<(TokenSlot, String)>` de saída (parse por linha com o estado
//!   carregando entre linhas — padrão do syntect p/ TUIs). A medição de
//!   scroll (`transcript_total_lines`) depende disso.
//! - **Cache LRU** por hash (linguagem, conteúdo): o parse é caro (blocos de
//!   centenas de linhas re-parseados a cada resize). Cap de 64 blocos; os
//!   slots são armazenados SEM cor (neutros de paleta), então o mesmo cache
//!   serve Dark/256/Retro/Light e os testes.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};

// ---------- slots semânticos (neutros de paleta) ----------

/// Papel semântico de um trecho de código. A cor REAL é escolhida pelo
/// `tui.rs` conforme a paleta ativa — este módulo nunca enxerga cores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSlot {
    /// Texto sem papel especial (variáveis, pontuação): fg herdado do papel.
    Plain,
    /// Palavras-chave e modificadores de armazenamento (`let`, `fn`, `int`).
    Keyword,
    /// Literais de string (e escapes dentro delas).
    Str,
    /// Comentários.
    Comment,
    /// Constantes: numéricos, `true`/`false`/`null`, caracteres.
    Const,
    /// Nomes de função/método (declaração ou chamada destacada).
    Func,
    /// Tipos/classes/structs (`entity.name.type`, `support.type`…).
    Type,
}

/// Tabela fixa scope→slot. ORDEM IMPORTA no empate: entradas mais abaixo são
/// mais específicas e vencem (mesma semântica de desempate dos temas do
/// syntect, onde regras posteriores vencem powers iguais).
const SCOPE_TABLE: &[(&str, TokenSlot)] = &[
    ("comment", TokenSlot::Comment),
    ("string", TokenSlot::Str),
    ("constant", TokenSlot::Const),
    ("keyword", TokenSlot::Keyword),
    ("storage", TokenSlot::Keyword),
    ("entity.name", TokenSlot::Type),
    ("support.type", TokenSlot::Type),
    ("support.function", TokenSlot::Func),
    ("entity.name.function", TokenSlot::Func),
    ("meta.function-call", TokenSlot::Func),
];

/// Seletores pré-parseados (1× por processo). Falha de parse de seletor
/// nosso (string fixa) seria bug de tabela — a entrada é simplesmente
/// ignorada (o trecho cai no fallback Plain).
fn selectors() -> &'static Vec<(ScopeStack, TokenSlot)> {
    static SEL: OnceLock<Vec<(ScopeStack, TokenSlot)>> = OnceLock::new();
    SEL.get_or_init(|| {
        SCOPE_TABLE
            .iter()
            .filter_map(|(sel, slot)| {
                ScopeStack::from_str(sel)
                    .ok()
                    .map(|s| (s, *slot))
            })
            .collect()
    })
}

/// Slot de um scope stack: maior MatchPower vence; empate fica com a entrada
/// mais específica (última da tabela com o mesmo power). Sem match → Plain.
fn slot_for(stack: &[Scope]) -> TokenSlot {
    let mut melhor = TokenSlot::Plain;
    let mut power_melhor: Option<f64> = None;
    for (sel, slot) in selectors() {
        if let Some(p) = sel.does_match(stack) {
            if power_melhor.is_none_or(|atual| p.0 >= atual) {
                power_melhor = Some(p.0);
                melhor = *slot;
            }
        }
    }
    melhor
}

// ---------- syntect (1× por processo) ----------

fn syntax_set() -> Option<&'static SyntaxSet> {
    static SS: OnceLock<Option<SyntaxSet>> = OnceLock::new();
    SS.get_or_init(|| {
        // Dumps embutidos (feature `default-syntaxes`). Falha de load → None
        // permanente: TODOS os blocos ficam crus (degradação total, honesta).
        Some(SyntaxSet::load_defaults_newlines())
    })
    .as_ref()
}

/// A linguagem declarada na fence é conhecida pelo syntect? (`rust`, `py`,
/// `bash`… — token = nome ou extensão, case-insensitive). Fences sem
/// linguagem NUNCA chegam aqui (o chamador mantém o caminho cru).
pub fn known_language(lang: &str) -> bool {
    let lang = lang.trim().to_lowercase();
    if lang.is_empty() {
        return false;
    }
    syntax_set()
        .is_some_and(|ss| ss.find_syntax_by_token(&lang).is_some())
}

// ---------- cache LRU (hash → blocos highlightados) ----------

/// Cap do cache LRU de blocos highlightados. Blocos de transcript são curtos
/// na prática; 64 blocos cobrem o histórico visível com folga.
const HL_CACHE_CAP: usize = 64;

/// Entrada: UMA `Vec<(TokenSlot, String)>` por linha do bloco (1:1 com
/// `code.lines()`), sem cor — a paleta é aplicada no consumo.
pub type HlBlock = Vec<Vec<(TokenSlot, String)>>;

struct HlCache {
    map: HashMap<u64, HlBlock>,
    ordem: VecDeque<u64>,
}

fn hl_cache() -> &'static Mutex<HlCache> {
    static CACHE: OnceLock<Mutex<HlCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(HlCache {
            map: HashMap::new(),
            ordem: VecDeque::new(),
        })
    })
}

/// Tamanho atual do cache (SÓ testes: prova o cap do LRU).
#[cfg(test)]
pub fn cache_len() -> usize {
    hl_cache().lock().map(|c| c.map.len()).unwrap_or(0)
}

fn cache_key(lang: &str, code: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    lang.to_lowercase().hash(&mut h);
    code.hash(&mut h);
    h.finish()
}

/// Move `key` para o fim da fila LRU (toque = uso recente).
fn tocar(ordem: &mut VecDeque<u64>, key: u64) {
    ordem.retain(|k| *k != key);
    ordem.push_back(key);
}

// ---------- API principal ----------

/// Realça um bloco INTEIRO: devolve `None` quando o syntect não tem a
/// sintaxe, falha no parse ou produz algo que fure o contrato 1:1 — o
/// chamador usa as linhas cruas atuais (bg `code_bg`, sem fg) nesses casos.
///
/// Contrato 1:1: `Some(saida)` tem `saida.len() == code.lines().count()` —
/// cada linha de entrada vira exatamente UMA lista de spans (slot, texto)
/// cujos textos concatenados recompõem a linha original (o wrap/medição
/// do transcript continua exato).
pub fn highlight_block(lang: &str, code: &str) -> Option<HlBlock> {
    let lang = lang.trim().to_lowercase();
    let ss = syntax_set()?;
    let syntax = ss.find_syntax_by_token(&lang)?;
    let key = cache_key(&lang, code);

    // Cache hit: move o bloco para o fim da fila (LRU de verdade) e devolve.
    if let Ok(mut c) = hl_cache().lock() {
        if let Some(entry) = c.map.get(&key) {
            let entry = entry.clone();
            tocar(&mut c.ordem, key);
            return Some(entry);
        }
    }

    // Parse por linha, com o estado carregando entre linhas. Com
    // `load_defaults_newlines` cada linha precisa do `\n` final (convenção
    // do syntect para sintaxes multiline: here-docs, strings com \n…).
    let mut state = ParseState::new(syntax);
    let mut saida: HlBlock = Vec::with_capacity(code.lines().count().max(1));
    for linha in code.lines() {
        let mut com_nl = String::with_capacity(linha.len() + 1);
        com_nl.push_str(linha);
        com_nl.push('\n');
        let ops = state.parse_line(&com_nl, ss).ok()?;
        let mut spans: Vec<(TokenSlot, String)> = Vec::new();
        // Replay dos ops (o que o HighlightIterator faz por dentro): o stack
        // VIGENTE antes de cada op pinta o trecho desde o fim do span
        // anterior. O stack é aplicado SEMPRE (estado da próxima linha), o
        // span só é emitido quando o índice cruza a linha real (o `\n`
        // sintético fica fora).
        let mut stack = ScopeStack::new();
        let mut ultimo = 0usize;
        for (idx_raw, op) in ops {
            let idx = idx_raw.min(linha.len());
            if idx > ultimo {
                let texto = &linha[ultimo..idx];
                if !texto.is_empty() {
                    // Merge de vizinhos iguais (menos spans no render).
                    if let Some((s_slot, s_txt)) = spans.last_mut() {
                        if *s_slot == slot_for(stack.as_slice()) {
                            s_txt.push_str(texto);
                        } else {
                            spans.push((slot_for(stack.as_slice()), texto.to_string()));
                        }
                    } else {
                        spans.push((slot_for(stack.as_slice()), texto.to_string()));
                    }
                }
                ultimo = idx;
            }
            stack.apply(&op).ok()?;
        }
        if ultimo < linha.len() {
            let texto = &linha[ultimo..];
            if !texto.is_empty() {
                spans.push((slot_for(stack.as_slice()), texto.to_string()));
            }
        }
        if spans.is_empty() {
            // Linha vazia: UMA saída, span vazio — o 1:1 nunca quebra (o
            // consumidor emite a Line vazia com o bg do código).
            spans.push((TokenSlot::Plain, String::new()));
        }
        saida.push(spans);
    }
    if saida.len() != code.lines().count() {
        return None; // defesa extra do 1:1 (não deveria acontecer)
    }

    if let Ok(mut c) = hl_cache().lock() {
        if c.map.len() >= HL_CACHE_CAP && !c.map.contains_key(&key) {
            // LRU simples: expulsa o mais antigo enquanto não couber.
            while c.map.len() >= HL_CACHE_CAP {
                match c.ordem.pop_front() {
                    Some(velho) => {
                        c.map.remove(&velho);
                    }
                    None => break,
                }
            }
        }
        c.map.insert(key, saida.clone());
        tocar(&mut c.ordem, key);
    }
    Some(saida)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_reconhecido_e_keyword_no_slot_certo() {
        assert!(known_language("rust"));
        assert!(known_language("Rust"));
        assert!(known_language("py"));
        assert!(known_language("bash"));
        assert!(!known_language(""));
        assert!(!known_language("zesperanto-que-nao-existe"));

        let blocos = highlight_block("rust", "let x = 1;\n").expect("rust parseia");
        assert_eq!(blocos.len(), 1, "1:1 linha→saída");
        let texto: String = blocos[0].iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(texto, "let x = 1;", "spans recompõem a linha");
        let keyword = blocos[0]
            .iter()
            .find(|(s, _)| *s == TokenSlot::Keyword)
            .expect("`let` é keyword/storage");
        assert_eq!(keyword.1, "let");
    }

    #[test]
    fn comentario_string_e_numero_têm_slots_distintos() {
        let blocos =
            highlight_block("python", "# nota\nx = \"texto\"  # fim\ny = 42\n")
                .expect("python parseia");
        assert_eq!(blocos.len(), 3);
        assert!(
            blocos[0]
                .iter()
                .any(|(s, t)| *s == TokenSlot::Comment && t.contains("nota")),
            "linha 1 tem comentário: {:?}",
            blocos[0]
        );
        assert!(
            blocos[1].iter().any(|(s, _)| *s == TokenSlot::Str),
            "linha 2 tem string: {:?}",
            blocos[1]
        );
        assert!(
            blocos[2].iter().any(|(s, _)| *s == TokenSlot::Const),
            "linha 3 tem número: {:?}",
            blocos[2]
        );
    }

    #[test]
    fn linguagem_desconhecida_e_vazia_devolvem_none() {
        assert!(highlight_block("zesperanto", "código").is_none());
        assert!(highlight_block("", "código").is_none());
    }

    #[test]
    fn linhas_vazias_multilinha_e_tab_mantem_1_para_1() {
        let code = "fn a() {\n\treturn 1;\n\n}\nlet largo = '日本語';\n";
        let blocos = highlight_block("rust", code).expect("parseia com tab/unicode");
        assert_eq!(
            blocos.len(),
            code.lines().count(),
            "1:1 com linha vazia no meio"
        );
        for (linha, spans) in code.lines().zip(&blocos) {
            let remontada: String = spans.iter().map(|(_, t)| t.as_str()).collect();
            assert_eq!(remontada, linha, "spans recompõem cada linha");
            for (_, t) in spans {
                assert!(t.is_char_boundary(0) && t.is_char_boundary(t.len()));
            }
        }
    }

    #[test]
    fn fence_sem_newline_final_também_1_para_1() {
        let blocos = highlight_block("rust", "let a = 1;\nlet b = 2;").expect("parseia");
        assert_eq!(blocos.len(), 2, "última linha sem \\n conta como linha");
    }

    #[test]
    fn cache_lru_respeita_o_cap() {
        // Blocos distintos (conteúdos diferentes) até estourar o cap; o
        // tamanho para em HL_CACHE_CAP (o mais antigo sai).
        for i in 0..(HL_CACHE_CAP + 10) {
            let code = format!("let v{i} = {i};\n");
            assert!(highlight_block("rust", &code).is_some());
        }
        assert_eq!(cache_len(), HL_CACHE_CAP, "cap do LRU");
    }

    #[test]
    fn cache_devolve_o_mesmo_conteudo_no_segundo_call() {
        let code = "fn cached() {}\n";
        let a = highlight_block("rust", code).expect("1ª passada");
        let b = highlight_block("rust", code).expect("2ª passada (cache)");
        assert_eq!(a, b);
    }

    #[test]
    fn slot_for_sem_match_cai_em_plain() {
        // Stack vazia/sem correspondência → Plain (fallback da tabela).
        assert_eq!(slot_for(&[]), TokenSlot::Plain);
    }

    #[test]
    fn escopo_mais_especifico_vence() {
        // "entity.name.function" (Func) vence "entity.name" (Type) na mesma
        // stack — MatchPower maior (mais átomos casados, mais fundo).
        let mut stack = ScopeStack::new();
        for escopo in ["source.rust", "meta.function", "entity.name.function"] {
            stack.push(Scope::from_str(escopo).expect("escopo de teste válido"));
        }
        assert_eq!(slot_for(stack.as_slice()), TokenSlot::Func);
        // Nada na tabela casa com attribute-name → Plain (fallback).
        let mut tipo = ScopeStack::new();
        tipo.push(Scope::from_str("source.py").unwrap());
        tipo.push(Scope::from_str("entity.other.attribute-name").unwrap());
        assert_eq!(slot_for(tipo.as_slice()), TokenSlot::Plain);
    }

    #[test]
    fn tabela_de_scopes_tem_10_entradas_validas() {
        // Toda entrada da tabela parseia como seletor (bug de tabela = teste
        // vermelho aqui, não silêncio em runtime).
        assert_eq!(SCOPE_TABLE.len(), 10);
        assert_eq!(selectors().len(), 10);
    }
}
