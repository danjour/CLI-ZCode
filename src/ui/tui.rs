//! TUI rica: estado puro e render ratatui.
//!
//! Shell de agente com header, corpo horizontal, sidebar, transcript,
//! prompt inferior e rodape. Streaming e permissoes reais seguem o
//! fluxo existente; esta camada altera somente a apresentacao.

use crate::session::{ContextUsage, MsgKind, TurnStats};
use crate::ui::art;
use crate::ui::theme::{self, Palette};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
    Frame,
};

// ---------- teclas ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiKey {
    Char(char),
    Newline, // Ctrl+J (Shift+Enter não chega ao terminal; documentado)
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    /// Home com Ctrl — mesmo efeito de Home (topo do transcript).
    CtrlHome,
    /// End com Ctrl — mesmo efeito de End (fim do transcript).
    CtrlEnd,
    /// Up com Ctrl — recall do histórico de prompts SEM restrição de linha
    /// (substitui o buffer mesmo multilinha).
    CtrlUp,
    /// Down com Ctrl — avanço no histórico de prompts SEM restrição de linha.
    CtrlDown,
    PageUp,
    PageDown,
    Esc,
    CtrlC,
    CtrlR, // resume (leitura R1)
    CtrlN, // new
    CtrlU, // usage refresh
    CtrlP, // permissão (stub Tempo 1)
    Ignore,
}

/// Mapeia evento crossterm → ação (puro, testável com fixtures).
/// `Release` é ignorado (cada tecla gerava Press + Release = char 2x);
/// `Press` e `Repeat` (tecla segurada) são mantidos.
pub fn map_key(ev: KeyEvent) -> TuiKey {
    if ev.kind == KeyEventKind::Release {
        return TuiKey::Ignore;
    }
    match (ev.code, ev.modifiers) {
        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => TuiKey::CtrlC,
        (KeyCode::Char('r'), m) if m.contains(KeyModifiers::CONTROL) => TuiKey::CtrlR,
        (KeyCode::Char('n'), m) if m.contains(KeyModifiers::CONTROL) => TuiKey::CtrlN,
        (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => TuiKey::CtrlU,
        (KeyCode::Char('p'), m) if m.contains(KeyModifiers::CONTROL) => TuiKey::CtrlP,
        (KeyCode::Char('j'), m) if m.contains(KeyModifiers::CONTROL) => TuiKey::Newline,
        (KeyCode::Enter, _) => TuiKey::Enter,
        (KeyCode::Esc, _) => TuiKey::Esc,
        (KeyCode::Backspace, _) => TuiKey::Backspace,
        (KeyCode::Delete, _) => TuiKey::Delete,
        (KeyCode::Left, _) => TuiKey::Left,
        (KeyCode::Right, _) => TuiKey::Right,
        // Ctrl+Up/Ctrl+Down antes dos casos genéricos de Up/Down (histórico).
        (KeyCode::Up, m) if m.contains(KeyModifiers::CONTROL) => TuiKey::CtrlUp,
        (KeyCode::Down, m) if m.contains(KeyModifiers::CONTROL) => TuiKey::CtrlDown,
        (KeyCode::Up, _) => TuiKey::Up,
        (KeyCode::Down, _) => TuiKey::Down,
        // Ctrl+Home/Ctrl+End antes dos casos genéricos de Home/End.
        (KeyCode::Home, m) if m.contains(KeyModifiers::CONTROL) => TuiKey::CtrlHome,
        (KeyCode::End, m) if m.contains(KeyModifiers::CONTROL) => TuiKey::CtrlEnd,
        (KeyCode::Home, _) => TuiKey::Home,
        (KeyCode::End, _) => TuiKey::End,
        (KeyCode::PageUp, _) => TuiKey::PageUp,
        (KeyCode::PageDown, _) => TuiKey::PageDown,
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            TuiKey::Char(c)
        }
        _ => TuiKey::Ignore,
    }
}

// ---------- markdown leve ----------

/// Uma linha com `**negrito**`, `` `código` `` e `# título` → spans.
/// Variante com fundo em blocos de código (polimento TUI visual).
pub fn markdown_spans_bg<'a>(line: &'a str, code_bg: Option<Color>) -> Vec<Span<'a>> {
    let code_style = match code_bg {
        Some(bg) => Style::default().bg(bg).add_modifier(Modifier::BOLD),
        None => Style::default().add_modifier(Modifier::REVERSED),
    };
    let mut out = Vec::new();
    let mut rest = line;
    // `# ` no início = título.
    if let Some(t) = rest.strip_prefix("# ") {
        out.push(Span::styled(
            t,
            Style::default().add_modifier(Modifier::BOLD),
        ));
        return out;
    }
    while !rest.is_empty() {
        if let Some(inner) = rest.strip_prefix("**") {
            match inner.find("**") {
                Some(end) => {
                    out.push(Span::styled(
                        &inner[..end],
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                    rest = &inner[end + 2..];
                }
                None => {
                    out.push(Span::raw(rest));
                    break;
                }
            }
        } else if let Some(inner) = rest.strip_prefix('`') {
            match inner.find('`') {
                Some(end) => {
                    out.push(Span::styled(&inner[..end], code_style));
                    rest = &inner[end + 1..];
                }
                None => {
                    out.push(Span::raw(rest));
                    break;
                }
            }
        } else {
            let next = rest
                .find("**")
                .into_iter()
                .chain(rest.find('`'))
                .min()
                .unwrap_or(rest.len());
            out.push(Span::raw(&rest[..next]));
            rest = &rest[next..];
        }
    }
    if out.is_empty() {
        out.push(Span::raw(""));
    }
    out
}

/// Linha de abertura/fechamento de fence de código: qualquer linha cujo
/// conteúdo (ignorando indentação) começa com ```. Simples de propósito —
/// não é um parser CommonMark (fences de 4+ crases/`~~~` ficam como texto).
fn linha_fence(raw: &str) -> bool {
    raw.trim_start().starts_with("```")
}

/// Pré-processamento markdown de UMA mensagem, LINHA a LINHA:
/// - blocos cercados por ``` (state machine "dentro de fence"): a linha da
///   fence é renderizada DIM/muted (a linguagem declarada em ```rust segue
///   visível, pois a própria fence cruza como texto) e as linhas internas
///   ganham bg = `code_bg` — o MESMO fundo do inline code — SEM processamento
///   de markdown, preservando espaços (o wrap continua por conta do
///   Paragraph);
/// - listas: `- `/`* ` → bullet `• ` com fg muted (mesma largura: 2 células);
///   numeradas `1. `/`12. ` mantêm o número e estilizam só o marcador;
///   indentação de sub-itens (2+ espaços) é preservada;
/// - títulos `## `/`### ` ganham BOLD com fg de destaque (system/badge_fg,
///   um por nível — interceptados aqui p/ ter a paleta à mão); `# ` segue o
///   tratamento original dentro de `markdown_spans_bg`.
/// Invariante do render: cada linha de entrada vira EXATAMENTE UMA sequência
/// de spans (uma `Line` na saída) — nada é juntado ou partido, senão a
/// medição de `transcript_total_lines` (`Paragraph::line_count`) diverge do
/// render real. O estado do fence vive só entre as linhas da própria
/// mensagem (reinicia a cada chamada = a cada mensagem).
fn markdown_message_spans<'a>(text: &'a str, pal: &Palette) -> Vec<Vec<Span<'a>>> {
    let fence_style = Style::default().fg(pal.muted).add_modifier(Modifier::DIM);
    let code_style = Style::default().bg(pal.code_bg);
    let marker_style = Style::default().fg(pal.muted);
    let mut em_fence = false;
    text.lines()
        .map(|raw| {
            if linha_fence(raw) {
                // Abre/fecha fence: alterna o estado e esmaece a própria
                // marca (```rust inclui a linguagem, que continua legível).
                em_fence = !em_fence;
                return vec![Span::styled(raw, fence_style)];
            }
            if em_fence {
                // Dentro do código: texto cru com bg, sem markdown.
                return vec![Span::styled(raw, code_style)];
            }
            let indent = raw.len() - raw.trim_start_matches(' ').len();
            let (indent_str, conteudo) = (&raw[..indent], &raw[indent..]);
            // Títulos com fg de destaque, um fg por nível (BOLD herdado do
            // tratamento de título; `##` sem espaço NÃO é título).
            for (marca, fg) in [("### ", pal.badge_fg), ("## ", pal.system)] {
                if let Some(t) = conteudo.strip_prefix(marca) {
                    let mut spans: Vec<Span> = Vec::new();
                    if !indent_str.is_empty() {
                        spans.push(Span::raw(indent_str));
                    }
                    spans.push(Span::styled(
                        t,
                        Style::default().fg(fg).add_modifier(Modifier::BOLD),
                    ));
                    return spans;
                }
            }
            // Bullet `- `/`* ` → `• ` (mesma largura em células).
            if let Some(resto) = conteudo
                .strip_prefix("- ")
                .or_else(|| conteudo.strip_prefix("* "))
            {
                let mut spans: Vec<Span> = Vec::new();
                if !indent_str.is_empty() {
                    spans.push(Span::raw(indent_str));
                }
                spans.push(Span::styled("• ", marker_style));
                spans.extend(markdown_spans_bg(resto, Some(pal.code_bg)));
                return spans;
            }
            // Numerada `12. `: o número permanece, só o marcador é estilizado.
            let digitos = conteudo.bytes().take_while(|b| b.is_ascii_digit()).count();
            if digitos > 0 && conteudo[digitos..].starts_with(". ") {
                let mut spans: Vec<Span> = Vec::new();
                if !indent_str.is_empty() {
                    spans.push(Span::raw(indent_str));
                }
                spans.push(Span::styled(&conteudo[..digitos + 2], marker_style));
                spans.extend(markdown_spans_bg(&conteudo[digitos + 2..], Some(pal.code_bg)));
                return spans;
            }
            // Linha comum: parser de linha único (título `# `/bold/inline).
            markdown_spans_bg(raw, Some(pal.code_bg))
        })
        .collect()
}

// ---------- permissão (stub Tempo 1) ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermStub {
    pub tool: String,
    pub detail: String,
}

/// Dados stub: componente existe, decisão não tem efeito (Tempo 2 liga o real).
/// Só fixture de teste hoje; o fluxo real de permissões é Tempo 2
/// (docs/RESUMO.md "Futuro registrado").
#[allow(dead_code)]
pub fn stub_permission() -> PermStub {
    PermStub {
        tool: "Bash(git *)".to_string(),
        detail: "stub Tempo 1 — y/n aqui não autoriza nada (Tempo 2 liga o real)".to_string(),
    }
}

// ---------- estado ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMsg {
    pub role: String,
    pub text: String,
    /// Reasoning vira mensagem própria (DIM + badge THINK) vinda do poller.
    pub kind: MsgKind,
}

#[derive(Debug)]
pub struct TuiApp {
    pub session_id: String,
    pub workspace: String,
    pub model: String,
    pub mode: String,
    pub messages: Vec<ChatMsg>,
    /// Versão do histórico: bump em toda mutação de `messages`. É a chave do
    /// cache do transcript (com largura/altura) — sem ela, qualquer draw
    /// re-parseava markdown de todo o histórico.
    pub messages_rev: u64,
    /// Cache do transcript renderizável: `(messages_rev, largura, altura) →
    /// linhas prontas`. Vive na task da UI (sem compartilhamento entre tasks).
    /// A linha de spinner "working…" é por-frame (o tick muda) e fica FORA.
    pub transcript_cache: Option<(u64, u16, u16, Vec<Line<'static>>)>,
    pub input: String,
    /// cursor em nº de chars (sempre em fronteira).
    pub cursor: usize,
    /// Offset de scroll a partir do TOPO do transcript (semântica
    /// `Paragraph::scroll`). Válido quando `follow == false`; o render grava
    /// de volta o offset aplicado (clampado) a cada frame.
    pub scroll: u16,
    /// Grudado no fim: novas mensagens ficam visíveis (comportamento de chat).
    /// `scroll_up` desliga; chegar ao fundo de novo religa.
    pub follow: bool,
    /// Altura interna real do transcript medida no último render — passo de
    /// página (PageUp/PageDown) em vez do passo fixo. 0 = ainda não renderizou.
    pub last_inner_height: u16,
    pub working: bool,
    pub status: String,
    pub tokens: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Métricas do último turno (contrato TUI-visual §1).
    pub last_stats: Option<TurnStats>,
    /// Contexto do create (contrato TUI-visual §2).
    pub ctx: ContextUsage,
    /// Contador de ticks (anima o spinner).
    pub tick: u64,
    pub pending_compact: bool,
    pub perm_stub: Option<PermStub>,
    pub ctrlc_once: bool,
    pub quit: bool,
    pub read_only: bool,
    /// Runtime Node caiu (`RuntimeError::Exited`): trava novos turnos e o
    /// header mostra "DEAD". Recuperado só reabrindo o CLI (sem auto-restart).
    pub runtime_dead: bool,
    /// Histórico de prompts enviados (mais antigo → mais novo) para o recall
    /// ↑/↓. Cap de 100 (`PROMPT_HISTORY_CAP`), dedupe de igual consecutivo.
    pub prompt_history: Vec<String>,
    /// Posição atual na navegação do histórico; `None` = editando livre.
    pub prompt_history_idx: Option<usize>,
    /// O que estava digitado quando o usuário subiu para o histórico;
    /// restaurado ao descer de volta além do mais novo.
    pub draft: Option<String>,
}

/// Cap do histórico de prompts (sem limite rígido, só esse teto sanitário).
pub const PROMPT_HISTORY_CAP: usize = 100;

impl TuiApp {
    pub fn new(session_id: &str, workspace: &str, read_only: bool) -> Self {
        Self {
            session_id: session_id.to_string(),
            workspace: workspace.to_string(),
            model: String::new(),
            mode: String::new(),
            messages: Vec::new(),
            messages_rev: 0,
            transcript_cache: None,
            input: String::new(),
            cursor: 0,
            scroll: 0,
            follow: true,
            last_inner_height: 0,
            working: false,
            status: "pronto".to_string(),
            tokens: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            last_stats: None,
            ctx: ContextUsage::default(),
            tick: 0,
            pending_compact: false,
            perm_stub: None,
            ctrlc_once: false,
            quit: false,
            read_only,
            runtime_dead: false,
            prompt_history: Vec::new(),
            prompt_history_idx: None,
            draft: None,
        }
    }

    pub fn push_msg(&mut self, role: &str, text: &str) {
        // Sem reset de scroll: com `follow` o render gruda no fim (offset =
        // total − altura da viewport); sem `follow`, o offset absoluto é
        // preservado (drift aceitável ao chegar mensagem nova).
        self.messages.push(ChatMsg {
            role: role.into(),
            text: text.into(),
            kind: MsgKind::Text,
        });
        self.bump_rev();
    }

    /// Bump manual da versão do histórico (clear/replace direto de
    /// `messages` — ex. `merge_messages`, Ctrl+N). Invalida o cache.
    pub fn bump_rev(&mut self) {
        self.messages_rev = self.messages_rev.wrapping_add(1);
    }

    // ----- scroll do transcript (offset a partir do TOPO) -----

    /// Sobe `n` linhas (conteúdo mais antigo) e desliga o follow. Parte do
    /// offset real gravado pelo render — o 1º salto a partir do fim funciona.
    pub fn scroll_up(&mut self, n: u16) {
        self.follow = false;
        self.scroll = self.scroll.saturating_sub(n);
    }

    /// Desce `n` linhas (conteúdo mais novo); o render religa o follow quando
    /// o offset clampado chega ao fundo.
    pub fn scroll_down(&mut self, n: u16) {
        if self.follow {
            return; // já grudado no fim
        }
        self.scroll = self.scroll.saturating_add(n);
    }

    /// Topo do transcript.
    pub fn scroll_home(&mut self) {
        self.follow = false;
        self.scroll = 0;
    }

    /// Fim do transcript (o render fixa o offset no fundo).
    pub fn scroll_end(&mut self) {
        self.follow = true;
    }

    /// Passo de página real (PageUp/PageDown): altura interna do transcript
    /// medida no último render, com 2 linhas de contexto visual; mínimo 1.
    /// Antes do 1º render (ou área zero) cai no passo fixo de 10 linhas.
    pub fn page_step(&self) -> u16 {
        if self.last_inner_height == 0 {
            10
        } else {
            self.last_inner_height.saturating_sub(2).max(1)
        }
    }

    /// Ctrl+C 2x sai: retorna true no 2º.
    pub fn ctrlc(&mut self) -> bool {
        if self.ctrlc_once {
            self.quit = true;
            true
        } else {
            self.ctrlc_once = true;
            self.status = "Ctrl+C de novo p/ sair (Esc cancela o turn).".to_string();
            false
        }
    }

    /// Qualquer outra tecla limpa o arme do Ctrl+C.
    pub fn disarm_ctrlc(&mut self) {
        self.ctrlc_once = false;
    }

    // ----- buffer multilinha (cursor em chars) -----

    fn cursor_byte(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.cursor)
            .map(|(b, _)| b)
            .unwrap_or_else(|| self.input.len())
    }

    pub fn insert_char(&mut self, c: char) {
        self.reset_prompt_nav();
        let b = self.cursor_byte();
        self.input.insert(b, c);
        self.cursor += 1;
    }

    pub fn newline(&mut self) {
        self.insert_char('\n');
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.reset_prompt_nav();
        let b = self.cursor_byte();
        let prev = self.input[..b].chars().next_back().unwrap();
        self.input.drain(b - prev.len_utf8()..b);
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        let b = self.cursor_byte();
        if b >= self.input.len() {
            return;
        }
        self.reset_prompt_nav();
        let next = self.input[b..].chars().next().unwrap();
        self.input.drain(b..b + next.len_utf8());
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        let n = self.input_len();
        self.cursor = (self.cursor + 1).min(n);
    }

    /// Número de unidades de edição do buffer (índice lógico, não largura visual).
    pub fn input_len(&self) -> usize {
        self.input.chars().count()
    }

    fn line_col(&self) -> (usize, usize) {
        let upto: String = self.input.chars().take(self.cursor).collect();
        let line = upto.matches('\n').count();
        let col = upto
            .rsplit('\n')
            .next()
            .map(|l| l.chars().count())
            .unwrap_or(0);
        (line, col)
    }

    fn line_start_char(&self, line: usize) -> usize {
        self.input
            .split('\n')
            .take(line)
            .map(|l| l.chars().count() + 1)
            .sum()
    }

    fn line_len_chars(&self, line: usize) -> usize {
        self.input
            .split('\n')
            .nth(line)
            .map(|l| l.chars().count())
            .unwrap_or(0)
    }

    pub fn move_up(&mut self) {
        let (line, col) = self.line_col();
        if line == 0 {
            return;
        }
        let target = self.line_len_chars(line - 1).min(col);
        self.cursor = self.line_start_char(line - 1) + target;
    }

    pub fn move_down(&mut self) {
        let (line, col) = self.line_col();
        let total = self.input.matches('\n').count();
        if line >= total {
            return;
        }
        let target = self.line_len_chars(line + 1).min(col);
        self.cursor = self.line_start_char(line + 1) + target;
    }

    /// Insere um texto colado na posição do cursor, em CHARs (mesma unidade
    /// dos métodos de edição). Quebras de linha do clipboard são saneadas
    /// (`\r\n`/`\r` → `\n`) e ficam NO buffer — paste NÃO envia nada; o
    /// usuário revisa e dá Enter.
    pub fn insert_str(&mut self, s: &str) {
        let clean = s.replace("\r\n", "\n").replace('\r', "\n");
        if clean.is_empty() {
            return;
        }
        self.reset_prompt_nav();
        let b = self.cursor_byte();
        self.input.insert_str(b, &clean);
        self.cursor += clean.chars().count();
    }

    // ----- histórico de prompts (↑/↓) -----

    /// Edição manual do buffer sai do modo de navegação do histórico (e
    /// descarta o rascunho preservado — ele já não corresponde ao estado).
    fn reset_prompt_nav(&mut self) {
        self.prompt_history_idx = None;
        self.draft = None;
    }

    /// Substitui o buffer inteiro (recall do histórico) com o cursor no fim.
    fn set_buffer(&mut self, s: String) {
        self.input = s;
        self.cursor = self.input_len();
    }

    /// Registra um prompt EFETIVAMENTE enviado (chamar no ramo de envio do
    /// Enter). Dedupe de igual consecutivo; cap de 100 (o mais antigo sai).
    /// Também encerra qualquer navegação em curso.
    pub fn remember_prompt(&mut self, text: &str) {
        if !self.prompt_history.last().is_some_and(|last| last == text) {
            self.prompt_history.push(text.to_string());
            if self.prompt_history.len() > PROMPT_HISTORY_CAP {
                self.prompt_history.remove(0);
            }
        }
        self.reset_prompt_nav();
    }

    /// Um passo para trás no histórico (buffer vazio/single-line pelo ↑;
    /// sempre pelo Ctrl+Up). Na 1ª subida a partir da edição livre, preserva
    /// o rascunho em `draft`; no mais antigo, fica parado.
    fn history_prev(&mut self) {
        if self.prompt_history.is_empty() {
            return;
        }
        let idx = match self.prompt_history_idx {
            None => {
                self.draft = Some(std::mem::take(&mut self.input));
                self.prompt_history.len() - 1
            }
            Some(i) => i.saturating_sub(1),
        };
        self.prompt_history_idx = Some(idx);
        let item = self.prompt_history[idx].clone();
        self.set_buffer(item);
    }

    /// Um passo para frente no histórico; além do mais novo restaura o
    /// `draft` e volta à edição livre. Sem navegação ativa, não faz nada
    /// (o chamador decide cair na navegação de cursor).
    fn history_next(&mut self) {
        let Some(i) = self.prompt_history_idx else {
            return;
        };
        let new_input = if i + 1 < self.prompt_history.len() {
            self.prompt_history_idx = Some(i + 1);
            self.prompt_history[i + 1].clone()
        } else {
            self.prompt_history_idx = None;
            self.draft.take().unwrap_or_default()
        };
        self.set_buffer(new_input);
    }

    /// Tecla ↑: recall de histórico APENAS com buffer vazio ou single-line
    /// (aí o cursor já está na 1ª/única linha). Buffer multilinha mantém a
    /// navegação de cursor existente (`move_up`, inalterada).
    pub fn up_key(&mut self) {
        if self.input.contains('\n') || self.prompt_history.is_empty() {
            self.move_up();
        } else {
            self.history_prev();
        }
    }

    /// Tecla ↓: com navegação ativa, avança no histórico (o fim restaura o
    /// draft); sem navegação, mantém a navegação de cursor (`move_down`).
    pub fn down_key(&mut self) {
        if self.prompt_history_idx.is_some() {
            self.history_next();
        } else {
            self.move_down();
        }
    }

    /// Ctrl+↑: recall SEM restrição de linha (substitui o buffer sempre).
    pub fn history_up_force(&mut self) {
        self.history_prev();
    }

    /// Ctrl+↓: avanço SEM restrição de linha; sem navegação ativa, no-op.
    pub fn history_down_force(&mut self) {
        self.history_next();
    }

    /// Rota y/n quando há confirmação pendente (compact ou perm stub).
    /// Retorna a decisão consumida, se alguma.
    pub fn confirm_pending(&mut self, yes: bool) -> Option<String> {
        if self.pending_compact {
            self.pending_compact = false;
            return Some(if yes {
                "compact:yes".into()
            } else {
                "compact:no".into()
            });
        }
        if self.perm_stub.is_some() {
            let tool = self.perm_stub.as_ref().unwrap().tool.clone();
            self.perm_stub = None;
            return Some(if yes {
                format!("perm:allow:{tool} (stub — sem efeito; Tempo 2 liga o real)")
            } else {
                format!("perm:deny:{tool} (stub — sem efeito; Tempo 2 liga o real)")
            });
        }
        None
    }

    /// Atualiza tokens + string legada (REPL usa `tokens`).
    pub fn set_usage(&mut self, input_tokens: u64, output_tokens: u64) {
        self.tokens_in = input_tokens;
        self.tokens_out = output_tokens;
        self.tokens = format!("in={input_tokens} out={output_tokens}");
    }
}

// ---------- peças visuais (puras, fixtures) ----------

/// Largura visual em células do terminal, conforme o mesmo modelo usado pelo
/// Ratatui para spans/graphemes. Nunca usar `chars().count()` para layout.
pub fn cell_width(text: &str) -> usize {
    Line::from(text).width()
}

/// Trunca em uma fronteira de grapheme e deixa um marcador explícito.
fn truncate_cells(text: &str, max_width: usize) -> String {
    truncate_cells_with_suffix(text, max_width, "...")
}

fn truncate_cells_with_suffix(text: &str, max_width: usize, suffix: &str) -> String {
    if max_width == 0 {
        return String::new();
    }
    if cell_width(text) <= max_width {
        return text.to_string();
    }
    let suffix_width = cell_width(suffix);
    if suffix_width >= max_width {
        return truncate_cells_ascii(suffix, max_width);
    }
    let target = max_width - suffix_width;
    let line = Line::from(text);
    let mut out = String::new();
    let mut used = 0;
    for grapheme in line.styled_graphemes(Style::default()) {
        let width = cell_width(grapheme.symbol);
        if used + width > target {
            break;
        }
        out.push_str(grapheme.symbol);
        used += width;
    }
    out.push_str(suffix);
    out
}

fn truncate_cells_ascii(text: &str, max_width: usize) -> String {
    text.chars().take(max_width).collect()
}

/// A mensagem-splash é só um marcador no histórico (`render::splash`); o
/// conteúdo visual (grade half-block truecolor) é resolvido aqui.
fn is_splash(text: &str) -> bool {
    text == crate::ui::art::SPLASH_MARKER
}

fn input_prefix(app: &TuiApp) -> String {
    app.input
        .chars()
        .take(app.cursor)
        .collect::<String>()
        .rsplit('\n')
        .next()
        .unwrap_or("")
        .to_string()
}

fn input_cursor_cell(app: &TuiApp) -> usize {
    cell_width(&input_prefix(app))
}

fn input_prompt_for_line(line: usize) -> &'static str {
    if line == 0 {
        theme::prompt()
    } else {
        "| "
    }
}

fn input_visual_cursor_cell(app: &TuiApp) -> usize {
    let (line, _) = app.line_col();
    cell_width(input_prompt_for_line(line)) + input_cursor_cell(app)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutMode {
    Full,
    Compact,
    Minimal,
}

fn layout_mode(area: Rect, _has_permission: bool) -> LayoutMode {
    // O modo full precisa de altura para header/body/prompt/footer e largura
    // para provar visualmente a composicao em colunas.
    if area.height >= 14 && area.width >= 60 {
        LayoutMode::Full
    } else if area.height >= 6 {
        LayoutMode::Compact
    } else {
        LayoutMode::Minimal
    }
}

/// Frames do spinner (braille): um por tick.
pub const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub fn spinner_frame(tick: u64) -> char {
    SPINNER_FRAMES[tick as usize % SPINNER_FRAMES.len()]
}

/// Sessão curta p/ títulos (`sess_abff…`).
pub fn short_session(id: &str) -> String {
    // Preserve the established 13-cell title shape: 12 identifier cells plus
    // the one-cell ellipsis suffix.
    const N: usize = 13;
    truncate_cells_with_suffix(id, N, "…")
}

/// Badge por role.
pub fn role_badge(role: &str) -> &'static str {
    match role {
        "user" => "USER",
        "assistant" => "ASSIST",
        _ => "SYS",
    }
}

/// Rótulo do badge incluindo o kind: reasoning assistant usa "THINK" no
/// lugar de "ASSIST" (o render preenche p/ a MESMA largura de células —
/// prefixo do wrap idêntico, sem desalinhar as continuation lines).
pub fn badge_label(role: &str, kind: MsgKind) -> &'static str {
    if kind == MsgKind::Reasoning {
        "THINK"
    } else {
        role_badge(role)
    }
}

/// Help bar (1 linha): atalhos principais.
pub fn help_bar() -> &'static str {
    "Enter envia · Ctrl+J linha · /compact y/n · Esc cancela · Ctrl+C 2x sai"
}

pub fn help_bar_for_width(width: usize) -> String {
    let choices = [
        help_bar(),
        "Enter | Ctrl+J | /compact y/n | Esc | Ctrl+C",
        "Enter | Ctrl+J | /compact | Esc/Ctrl+C",
        "Enter|Ctrl+J|/compact|Esc/^C",
        "E|^J|/compact|Esc|^C",
        "?",
    ];
    choices
        .iter()
        .find(|text| cell_width(text) <= width)
        .map(|text| (*text).to_string())
        .unwrap_or_default()
}

fn confirmation_help_for_width(width: usize) -> String {
    ["y confirma | n cancela", "y sim | n nao", "y/n", "?"]
        .iter()
        .find(|text| cell_width(text) <= width)
        .map(|text| (*text).to_string())
        .unwrap_or_default()
}

fn help_for_app(app: &TuiApp, width: usize) -> String {
    if app.pending_compact || app.perm_stub.is_some() {
        confirmation_help_for_width(width)
    } else {
        help_bar_for_width(width)
    }
}

// ---------- render ----------

/// Intervalo do tick de animação (spinner/relógio/statusbar) — separado do
/// cadence de rede: 10 fps de animação enquanto `working`, sem custo de RPC.
/// O draw só acontece com a UI "dirty" (ver loop em `commands::run_tui`).
pub const ANIM_TICK_MS: u64 = 100;

fn role_color(role: &str, pal: &Palette) -> ratatui::style::Color {
    match role {
        "user" => pal.user,
        "assistant" => pal.assistant,
        _ => pal.system,
    }
}

/// Linha de spinner "working…" do transcript — POR FRAME (o tick anima),
/// logo fica fora do cache do transcript.
fn working_line(app: &TuiApp, pal: &Palette) -> Line<'static> {
    Line::from(Span::styled(
        format!("{} working… (Esc cancela)", spinner_frame(app.tick)),
        Style::default().fg(pal.system).add_modifier(Modifier::BOLD),
    ))
}

/// Transcript da viewport COM CACHE por `(messages_rev, largura, altura)`:
/// sem mudança de histórico ou de geometria, nenhum re-parse de markdown nem
/// re-amostragem da splash (o mesmo contrato de cache de `ui::art`).
fn transcript_text(
    app: &mut TuiApp,
    pal: &Palette,
    transcript_width: usize,
    transcript_height: u16,
) -> Text<'static> {
    let key = (app.messages_rev, transcript_width as u16, transcript_height);
    let hit = app
        .transcript_cache
        .as_ref()
        .is_some_and(|c| (c.0, c.1, c.2) == key);
    let mut lines = if hit {
        match &app.transcript_cache {
            Some(c) => c.3.clone(),
            None => Vec::new(),
        }
    } else {
        let built = history_lines(app, pal, transcript_width, transcript_height, false);
        app.transcript_cache = Some((key.0, key.1, key.2, built.clone()));
        built
    };
    if lines.is_empty() {
        // O footer concentra os atalhos; o histórico vazio não os duplica.
        lines.push(Line::from(Span::styled(
            "Nenhuma mensagem ainda.",
            Style::default().fg(pal.system).add_modifier(Modifier::BOLD),
        )));
    }
    if app.working {
        lines.push(working_line(app, pal));
    }
    Text::from(lines)
}

/// Linhas do histórico (OWNED — `Vec<Line<'static>>` é o que o cache guarda;
/// o render empresta por um frame). Saída visual idêntica à anterior; mudou
/// só a posse das strings. A linha de "working" NÃO entra aqui (por frame).
fn history_lines(
    app: &TuiApp,
    pal: &Palette,
    history_width: usize,
    history_height: u16,
    include_splash: bool,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut has_conversation_message = false;
    let notice_style = Style::default().fg(pal.system).add_modifier(Modifier::DIM);
    for m in &app.messages {
        if is_splash(&m.text) {
            if !include_splash {
                continue;
            }
            // Conteúdo visual da splash: grade half-block com fg+bg, amostrada
            // para a largura interna real e recortada à altura disponível.
            lines.extend(art::art_text(history_width as u16, history_height).lines);
            continue;
        }
        if m.role == "system" {
            // Notices de boot/estado formam um grupo contínuo: sem SYS, badge
            // ou separador. O marcador discreto preserva a leitura sem
            // competir com user/assistant.
            let mut notice_lines = m.text.lines();
            if let Some(first) = notice_lines.next() {
                lines.push(Line::from(vec![
                    Span::styled("· ", notice_style),
                    Span::styled(first.to_string(), notice_style),
                ]));
                for continuation in notice_lines {
                    lines.push(Line::from(vec![
                        Span::styled("  ", notice_style),
                        Span::styled(continuation.to_string(), notice_style),
                    ]));
                }
            } else {
                lines.push(Line::from(Span::styled("·", notice_style)));
            }
            continue;
        }
        if has_conversation_message {
            lines.push(Line::from(Span::styled(
                "─".repeat(history_width.saturating_sub(4).min(32)),
                Style::default().fg(pal.sep),
            )));
        }
        has_conversation_message = true;
        let badge_style = Style::default()
            .fg(pal.badge_fg)
            .add_modifier(Modifier::BOLD);
        let is_reasoning = m.kind == MsgKind::Reasoning;
        // THINK tem 5 células vs ASSIST 6: preenche p/ 6 → badge com a MESMA
        // largura de prefixo (wrap das continuation lines fica alinhado).
        let label = badge_label(&m.role, m.kind);
        let badge = if is_reasoning {
            format!(" {:<6} ", label)
        } else {
            format!(" {} ", label)
        };
        // Reasoning: DIM (e itálico — terminais sem itálico simplesmente
        // ignoram) + cor muted da paleta (distingue do assistant normal).
        let base = if is_reasoning {
            Style::default()
                .fg(pal.muted)
                .add_modifier(Modifier::DIM | Modifier::ITALIC)
        } else {
            Style::default().fg(role_color(&m.role, pal))
        };
        // Markdown rico POR MENSAGEM: o estado do fence vive entre as linhas
        // da própria mensagem e cada linha de entrada continua virando
        // exatamente 1 Line (a medição do Paragraph depende disso).
        let rendered = markdown_message_spans(&m.text, pal);
        if rendered.is_empty() {
            lines.push(Line::from(Span::styled(badge, badge_style)));
            continue;
        }
        for (i, md) in rendered.iter().enumerate() {
            let mut spans = if i == 0 {
                vec![Span::styled(badge.clone(), badge_style), Span::raw(" ")]
            } else {
                vec![Span::raw("   ")]
            };
            for s in md {
                // owned: Cow::clone manteria o borrow do histórico; o cache
                // precisa de Line<'static>. `base.patch(s.style)` deixa os
                // fg/bg EXPLÍCITOS do markdown sobreviverem ao estilo base
                // do papel — necessário p/ o bg do código e os fg de
                // títulos/fence/bullets. Em V≤1 o patch invertido
                // (`s.style.patch(base)`) fazia o estilo base vencer: o fg
                // explícito do markdown se perdia (título ficava na cor do
                // papel) e o bg do código só sobrevivia porque o base não
                // tinha bg. NÃO é bit-a-bit idêntico ao anterior — mudança
                // DELIBERADA nesta rodada (spans sem fg/bg próprio, ex.
                // `**bold**` e `` `inline` ``, seguem com o resultado antigo).
                spans.push(Span::styled(s.content.to_string(), base.patch(s.style)));
            }
            lines.push(Line::from(spans));
        }
    }
    if lines.is_empty() {
        // O footer concentra os atalhos; o histórico vazio não os duplica.
        lines.push(Line::from(Span::styled(
            "Nenhuma mensagem ainda.",
            Style::default().fg(pal.system).add_modifier(Modifier::BOLD),
        )));
    }
    lines
}

impl TuiApp {
    fn compact_status_line(&self) -> String {
        // DEAD domina (igual ao header): tela mínima também sinaliza a queda.
        let state = if self.runtime_dead {
            "DEAD"
        } else if self.working {
            "working"
        } else {
            "pronto"
        };
        format!(
            "{state} | ctx={:.0}% | model={} | {}",
            self.ctx.pct(),
            self.model,
            self.status
        )
    }
}

fn input_title(app: &TuiApp, compact: bool, width: usize) -> String {
    let title = if app.pending_compact {
        "compact: y confirma / n cancela"
    } else if app.perm_stub.is_some() && compact {
        "permissao: y confirma / n cancela"
    } else if app.working {
        "input: aguarde; Esc cancela"
    } else {
        "input: Enter envia; Ctrl+J quebra linha"
    };
    truncate_cells(title, width.saturating_sub(2))
}

fn input_text<'a>(app: &'a TuiApp, pal: &Palette) -> Text<'a> {
    let prompt_style = Style::default().fg(pal.prompt).add_modifier(Modifier::BOLD);
    let mut lines = app.input.split('\n');
    let first = lines.next().unwrap_or("");
    let mut rendered = vec![Line::from(vec![
        Span::styled(input_prompt_for_line(0), prompt_style),
        Span::raw(first),
    ])];
    for (line_number, raw) in lines.enumerate() {
        rendered.push(Line::from(vec![
            Span::styled(input_prompt_for_line(line_number + 1), prompt_style),
            Span::raw(raw),
        ]));
    }
    Text::from(rendered)
}

fn permission_compact_text(app: &TuiApp, width: usize) -> String {
    let Some(permission) = &app.perm_stub else {
        return String::new();
    };
    truncate_cells(&format!("permitir {}? y/n", permission.tool), width)
}

fn header_text(app: &TuiApp, width: usize) -> String {
    // Runtime morto domina o estado: o usuário precisa ver que NÃO dá p/ enviar.
    let state = if app.runtime_dead {
        "DEAD"
    } else if app.working {
        "working"
    } else {
        "ready"
    };
    let mode = if app.mode.is_empty() {
        "-"
    } else {
        app.mode.as_str()
    };
    let model = if app.model.is_empty() {
        "-"
    } else {
        app.model.as_str()
    };
    truncate_cells(
        &format!(
            " zcode-cli | session={} | mode={} | model={} | state={} ",
            short_session(&app.session_id),
            mode,
            model,
            state
        ),
        width,
    )
}

fn sidebar_width_for(width: u16) -> u16 {
    if width >= 120 {
        32
    } else {
        28
    }
}

fn compact_count(value: u64, reference: u64) -> String {
    let (scale, suffix) = if reference >= 1_000_000 {
        (1_000_000.0, "M")
    } else if reference >= 1_000 {
        (1_000.0, "K")
    } else {
        (1.0, "")
    };
    if suffix.is_empty() {
        value.to_string()
    } else {
        format!("{:.2}{suffix}", value as f64 / scale)
    }
}

fn compact_token_count(value: u64, reference: u64) -> String {
    compact_count(value, reference)
}

fn compact_context_text(ctx: &ContextUsage) -> String {
    format!(
        "{}/{} ({:.0}%)",
        compact_count(ctx.used, ctx.window),
        compact_count(ctx.window, ctx.window),
        ctx.pct()
    )
}

fn token_speed_text(app: &TuiApp) -> String {
    match app.last_stats.as_ref() {
        Some(stats) if stats.tok_per_s.is_finite() => format!("{:.1} tok/s", stats.tok_per_s),
        _ => "- tok/s".to_string(),
    }
}

fn sidebar_telemetry_text(app: &TuiApp, width: usize) -> String {
    let total_tokens = app.tokens_in.saturating_add(app.tokens_out);
    let mut lines = vec![
        truncate_cells(&format!("workspace {}", app.workspace), width),
        truncate_cells(
            &format!("context {}", compact_context_text(&app.ctx)),
            width,
        ),
        truncate_cells(
            &format!(
                "tokens total {}",
                compact_token_count(total_tokens, total_tokens)
            ),
            width,
        ),
        truncate_cells(
            &format!("input {}", compact_token_count(app.tokens_in, total_tokens)),
            width,
        ),
        truncate_cells(
            &format!(
                "output {}",
                compact_token_count(app.tokens_out, total_tokens)
            ),
            width,
        ),
        truncate_cells(&token_speed_text(app), width),
    ];
    if let Some(permission) = &app.perm_stub {
        lines.extend([
            "".to_string(),
            "permission".to_string(),
            truncate_cells(&permission.tool, width),
            "y/n confirma".to_string(),
        ]);
    }
    lines.join("\n")
}

fn render_header(f: &mut Frame, app: &TuiApp, pal: &Palette, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // Runtime morto: header na cor de erro/aviso da paleta (system — a mesma
    // das notices) + negrito; estado normal mantém o assistant.
    let style = if app.runtime_dead {
        Style::default()
            .fg(pal.system)
            .bg(pal.background)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(pal.assistant).bg(pal.background)
    };
    let text = header_text(app, area.width as usize);
    if area.height >= 2 {
        f.render_widget(
            Paragraph::new(text).style(style).block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(pal.border))
                    .style(Style::default().bg(pal.background)),
            ),
            area,
        );
    } else {
        f.render_widget(Paragraph::new(text).style(style), area);
    }
}

/// Painel do olho na sidebar: 12 linhas da arte compacta + 2 de bordas.
const SIDE_ART_PANEL_HEIGHT: u16 = 14;
/// Telemetria mínima que precisa sobrar para o painel do olho entrar em cena
/// (abaixo disso a arte some e a telemetria ocupa a sidebar inteira —
/// degradação graciosa em telas baixas, sem panic).
const SIDE_TELEMETRY_MIN_HEIGHT: u16 = 4;

fn render_sidebar(f: &mut Frame, app: &TuiApp, pal: &Palette, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // O olho só entra quando cabe INTEIRO (14 linhas) e ainda sobra telemetria
    // visível; em áreas menores o painel é escondido, o resto permanece.
    let art_height = if area.height >= SIDE_ART_PANEL_HEIGHT + SIDE_TELEMETRY_MIN_HEIGHT {
        SIDE_ART_PANEL_HEIGHT
    } else {
        0
    };
    let telemetry_height = area.height.saturating_sub(art_height);
    let side_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(art_height),
            Constraint::Length(telemetry_height),
        ])
        .split(area);

    if side_chunks[0].height > 0 {
        // Grade half-block truecolor do olho (fg+bg por célula), amostrada
        // para a área interna real; se sobrar menos arte do que o natural, o
        // recorte central do módulo evita panic.
        let body = art::side_text(
            side_chunks[0].width.saturating_sub(2),
            side_chunks[0].height.saturating_sub(2),
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(pal.frame)
            .border_style(Style::default().fg(pal.border))
            .style(Style::default().bg(pal.surface));
        f.render_widget(
            Paragraph::new(body)
                .block(block)
                .style(Style::default().bg(pal.surface))
                .wrap(Wrap { trim: false }),
            side_chunks[0],
        );
    }

    if side_chunks[1].height > 0 {
        let inner_width = side_chunks[1].width.saturating_sub(2) as usize;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(pal.frame)
            .border_style(Style::default().fg(pal.border))
            .title(" workspace / context ")
            .title_style(Style::default().fg(pal.user).add_modifier(Modifier::BOLD))
            .style(Style::default().bg(pal.surface));
        f.render_widget(
            Paragraph::new(sidebar_telemetry_text(app, inner_width))
                .block(block)
                .style(Style::default().fg(pal.assistant).bg(pal.surface)),
            side_chunks[1],
        );
    }
}

/// Nº de linhas visuais do texto com word-wrap na largura interna dada.
/// Usa a MESMA quebra do `Paragraph` do transcript (`WordWrapper`,
/// `trim = false`): em 0.29 o composer é privado, mas `Paragraph::line_count`
/// (feature `unstable-rendered-line-info`) roda exatamente o mesmo algoritmo —
/// contagem igual ao render por construção (provado por teste de propriedade
/// contra o render real em `TestBackend`).
/// `inner_width == 0` → 0.
pub fn transcript_total_lines(text: &Text, inner_width: u16) -> u16 {
    if inner_width == 0 {
        return 0;
    }
    // `Paragraph::new` exige `Text` por valor: reconstrói por referência
    // (spans emprestados, sem copiar strings) só para a medição — estilos e
    // alinhamento não afetam o wrap.
    let medido = Text::from(
        text.lines
            .iter()
            .map(|l| {
                Line::from(
                    l.spans
                        .iter()
                        .map(|s| Span::raw(s.content.as_ref()))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>(),
    );
    let total = Paragraph::new(medido)
        .wrap(Wrap { trim: false })
        .line_count(inner_width);
    total.min(u16::MAX as usize) as u16 // mesma saturação do acumulador antigo
}

/// Offset de scroll válido: nunca passa do fim (`total − inner_height`).
pub fn clamp_scroll(scroll: u16, total: u16, inner_height: u16) -> u16 {
    scroll.min(total.saturating_sub(inner_height))
}

fn render_transcript(f: &mut Frame, app: &mut TuiApp, pal: &Palette, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let title = format!(" transcript / {} ", short_session(&app.session_id));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(pal.frame)
        .border_style(Style::default().fg(pal.border))
        .title(title)
        .title_style(Style::default().fg(pal.user).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(pal.surface))
        .padding(Padding::horizontal(1));
    // Largura/altura internas REAIS (bordas + padding horizontal) — a MESMA
    // largura usada no wrap do texto e na medição de altura.
    let inner_width = area.width.saturating_sub(4);
    let inner_height = area.height.saturating_sub(2);
    let text = transcript_text(app, pal, inner_width as usize, inner_height);
    let total = transcript_total_lines(&text, inner_width);
    // `scroll` é offset a partir do TOPO: o fundo é `total − inner_height`.
    // follow gruda nele; sem follow, o offset pedido é clampado.
    let max_offset = total.saturating_sub(inner_height);
    let offset = if app.follow {
        max_offset
    } else {
        clamp_scroll(app.scroll, total, inner_height)
    };
    let transcript = Paragraph::new(text)
        .block(block)
        .style(Style::default().fg(pal.assistant).bg(pal.surface))
        .wrap(Wrap { trim: false })
        .scroll((offset, 0));
    f.render_widget(transcript, area);
    // Write-back APÓS o render (o texto emprestado de `app` já foi consumido):
    // o handler lê a posição real daqui (ex.: 1º PageUp a partir do fim).
    app.scroll = offset;
    if !app.follow && offset >= max_offset {
        app.follow = true; // desceu até o fundo → follow re-engaja
    }
    // Write-back ADICIONAL da altura interna real: vira o passo de página
    // (PageUp/PageDown). O write-back de `scroll` acima não muda.
    app.last_inner_height = inner_height;
    // Scrollbar na borda direita (só quando o conteúdo transborda; o margin
    // evita sobrescrever as quinas da borda em áreas pequenas).
    if total > inner_height {
        let mut sb_state = ScrollbarState::new(max_offset as usize).position(offset as usize);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .style(Style::default().fg(pal.border).bg(pal.surface)),
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut sb_state,
        );
    }
}

fn render_prompt(f: &mut Frame, app: &TuiApp, pal: &Palette, area: Rect, compact: bool) {
    render_input(f, app, pal, area, compact);
}

fn render_footer(f: &mut Frame, app: &TuiApp, pal: &Palette, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let style = Style::default().fg(pal.muted).bg(pal.background);
    let text = help_for_app(app, area.width as usize);
    if area.height >= 2 {
        f.render_widget(
            Paragraph::new(text).style(style).block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(pal.sep))
                    .style(Style::default().bg(pal.background)),
            ),
            area,
        );
    } else {
        f.render_widget(Paragraph::new(text).style(style), area);
    }
}

fn render_input(f: &mut Frame, app: &TuiApp, pal: &Palette, area: Rect, compact: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let bordered = area.width >= 4 && area.height >= 3;
    let inner_width = if bordered {
        area.width.saturating_sub(4) as usize
    } else {
        area.width as usize
    };
    let inner_height = if bordered {
        area.height.saturating_sub(2) as usize
    } else {
        area.height as usize
    };
    let (line, _) = app.line_col();
    let cursor_cell = input_visual_cursor_cell(app);
    let horizontal_scroll = cursor_cell.saturating_sub(inner_width.saturating_sub(1));
    let vertical_scroll = line.saturating_sub(inner_height.saturating_sub(1));
    let title = input_title(app, compact, area.width as usize);
    let mut input = Paragraph::new(input_text(app, pal))
        .style(Style::default().fg(pal.assistant).bg(pal.surface))
        .scroll((
            vertical_scroll.min(u16::MAX as usize) as u16,
            horizontal_scroll.min(u16::MAX as usize) as u16,
        ));
    if bordered {
        input = input.block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(pal.frame)
                .border_style(Style::default().fg(pal.border))
                .title(title)
                .title_style(Style::default().fg(pal.prompt).add_modifier(Modifier::BOLD))
                .style(Style::default().bg(pal.surface))
                .padding(Padding::horizontal(1)),
        );
    }
    f.render_widget(input, area);

    let x = cursor_cell
        .saturating_sub(horizontal_scroll)
        .min(inner_width.saturating_sub(1));
    let y = line
        .saturating_sub(vertical_scroll)
        .min(inner_height.saturating_sub(1));
    let origin_x = area.x + if bordered { 2 } else { 0 };
    let origin_y = area.y + if bordered { 1 } else { 0 };
    f.set_cursor_position((origin_x + x as u16, origin_y + y as u16));
}

fn render_minimal(f: &mut Frame, app: &TuiApp, pal: &Palette) {
    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let primary = if app.pending_compact {
        "compact: y confirma / n cancela".to_string()
    } else if app.perm_stub.is_some() {
        permission_compact_text(app, area.width as usize)
    } else if app.working {
        "working (Esc cancela)".to_string()
    } else if app.input.is_empty() {
        format!("{}Enter envia | Esc sai", theme::prompt())
    } else {
        format!("{}{}", theme::prompt(), input_prefix(app))
    };
    let mut lines = vec![Line::from(truncate_cells(&primary, area.width as usize))];
    if area.height >= 2 {
        lines.push(Line::from(truncate_cells(
            &format!("status: {}", app.compact_status_line()),
            area.width as usize,
        )));
    }
    f.render_widget(
        Paragraph::new(Text::from(lines)).style(Style::default().fg(pal.system).bg(pal.background)),
        area,
    );
    let x = input_visual_cursor_cell(app).min(area.width.saturating_sub(1) as usize) as u16;
    f.set_cursor_position((area.x + x, area.y));
}

/// Composição Debian: chrome superior, corpo em colunas, prompt e rodapé.
pub fn render(f: &mut Frame, app: &mut TuiApp, pal: &Palette) {
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::default().bg(pal.background)),
        area,
    );
    match layout_mode(area, app.perm_stub.is_some()) {
        LayoutMode::Full => render_full(f, app, pal),
        LayoutMode::Compact => render_compact(f, app, pal),
        LayoutMode::Minimal => render_minimal(f, app, pal),
    }
}

fn render_full(f: &mut Frame, app: &mut TuiApp, pal: &Palette) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(f.area());
    render_header(f, app, pal, chunks[0]);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(sidebar_width_for(chunks[1].width)),
            Constraint::Min(1),
        ])
        .split(chunks[1]);
    render_sidebar(f, app, pal, body[0]);
    render_transcript(f, app, pal, body[1]);
    render_prompt(f, app, pal, chunks[2], false);
    render_footer(f, app, pal, chunks[3]);
}

fn render_compact(f: &mut Frame, app: &mut TuiApp, pal: &Palette) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(f.area());
    render_header(f, app, pal, chunks[0]);
    render_transcript(f, app, pal, chunks[1]);
    render_prompt(f, app, pal, chunks[2], true);
    render_footer(f, app, pal, chunks[3]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventKind, KeyEventState};

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn key_kind(code: KeyCode, mods: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn teclas_fase4_fixtures() {
        use KeyCode as K;
        use KeyModifiers as M;
        assert_eq!(map_key(key(K::Char('c'), M::CONTROL)), TuiKey::CtrlC);
        assert_eq!(map_key(key(K::Char('r'), M::CONTROL)), TuiKey::CtrlR);
        assert_eq!(map_key(key(K::Char('n'), M::CONTROL)), TuiKey::CtrlN);
        assert_eq!(map_key(key(K::Char('u'), M::CONTROL)), TuiKey::CtrlU);
        assert_eq!(map_key(key(K::Char('p'), M::CONTROL)), TuiKey::CtrlP);
        assert_eq!(map_key(key(K::Char('j'), M::CONTROL)), TuiKey::Newline);
        assert_eq!(map_key(key(K::Enter, M::empty())), TuiKey::Enter);
        assert_eq!(map_key(key(K::Esc, M::empty())), TuiKey::Esc);
        assert_eq!(map_key(key(K::Char('a'), M::empty())), TuiKey::Char('a'));
    }

    #[test]
    fn release_ignorado_press_repeat_mantidos() {
        use KeyCode as K;
        use KeyModifiers as M;
        // Bug: Release processado como Press duplicava cada tecla.
        for code in [K::Char('a'), K::Char(' '), K::Enter, K::Backspace] {
            assert_eq!(
                map_key(key_kind(code, M::empty(), KeyEventKind::Release)),
                TuiKey::Ignore,
                "{code:?}"
            );
        }
        // Press e Repeat (tecla segurada) mantidos.
        assert_eq!(
            map_key(key_kind(K::Char('a'), M::empty(), KeyEventKind::Press)),
            TuiKey::Char('a')
        );
        assert_eq!(
            map_key(key_kind(K::Char('a'), M::empty(), KeyEventKind::Repeat)),
            TuiKey::Char('a')
        );
        assert_eq!(
            map_key(key_kind(K::Enter, M::empty(), KeyEventKind::Repeat)),
            TuiKey::Enter
        );
    }

    #[test]
    fn buffer_multilinha() {
        let mut a = TuiApp::new("s", "w", false);
        for c in "oi".chars() {
            a.insert_char(c);
        }
        a.newline();
        for c in "x".chars() {
            a.insert_char(c);
        }
        assert_eq!(a.input, "oi\nx");
        a.move_up(); // linha 0, col 1 (min(2,1))
        a.insert_char('!');
        assert_eq!(a.input, "o!i\nx");
        a.move_down();
        a.backspace();
        assert_eq!(a.input, "o!i\n");
    }

    #[test]
    fn markdown_leve() {
        let spans = markdown_spans_bg("olá **mundo** e `code`", None);
        let texts: Vec<String> = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(texts.join(""), "olá mundo e code");
        assert!(spans.iter().any(|s| s.style.add_modifier == Modifier::BOLD));
        let h = markdown_spans_bg("# t", None);
        assert!(h.iter().any(|s| s.style.add_modifier == Modifier::BOLD));
    }

    // ----- markdown rico: fences de código, listas e títulos -----

    #[test]
    fn markdown_rico_fence_abre_e_fecha() {
        use crate::ui::theme::Theme;
        let pal = Theme::Dark.palette();
        let msg = "antes\n```rust\nlet x = 1;\n```\ndepois `code`";
        let rendered = markdown_message_spans(msg, &pal);
        // Invariante do render: 1 linha de entrada → 1 Line de saída.
        assert_eq!(rendered.len(), msg.lines().count());
        let texto =
            |l: &[Span]| l.iter().map(|s| s.content.as_ref()).collect::<String>();
        // Fence de abertura DIM/muted; a linguagem segue visível (a marca
        // cruza como texto).
        let abre = &rendered[1];
        assert_eq!(texto(abre), "```rust");
        assert_eq!(abre[0].style.fg, Some(pal.muted));
        assert!(abre[0].style.add_modifier.contains(Modifier::DIM));
        // Dentro do fence: SEM markdown, bg = code_bg (o mesmo do inline).
        let codigo = &rendered[2];
        assert_eq!(texto(codigo), "let x = 1;");
        assert_eq!(codigo[0].style.bg, Some(pal.code_bg));
        assert_eq!(codigo[0].style.fg, None, "código não recebe markdown");
        // Fence fechada: markdown volta a valer na linha seguinte.
        let fecha = &rendered[3];
        assert_eq!(texto(fecha), "```");
        assert!(fecha[0].style.add_modifier.contains(Modifier::DIM));
        let depois = &rendered[4];
        assert_eq!(texto(depois), "depois code");
        assert!(depois.iter().any(|s| s.style.bg == Some(pal.code_bg)));
        // Antes da fence: linha comum sem bg.
        assert!(rendered[0].iter().all(|s| s.style.bg.is_none()));
    }

    #[test]
    fn markdown_rico_fence_nao_fechada_vai_ate_o_fim() {
        use crate::ui::theme::Theme;
        let pal = Theme::Dark.palette();
        let msg = "```py\nprint(1)\n    indentado";
        let rendered = markdown_message_spans(msg, &pal);
        assert_eq!(rendered.len(), 3, "1:1 mesmo com fence aberta");
        // Fence não fechada: TODO o resto da mensagem é código.
        assert!(rendered[1..]
            .iter()
            .all(|l| l.iter().all(|s| s.style.bg == Some(pal.code_bg))));
        // Espaços internos preservados (sem trim/parse de markdown).
        let texto =
            |l: &[Span]| l.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert_eq!(texto(&rendered[2]), "    indentado");
        // Estado NÃO vaza para a mensagem seguinte (chamada nova = msg nova).
        let proxima = markdown_message_spans("texto normal", &pal);
        assert!(proxima[0].iter().all(|s| s.style.bg.is_none()));
    }

    #[test]
    fn markdown_rico_fence_em_reasoning_mantem_dim_e_think() {
        use crate::ui::theme::Theme;
        let pal = Theme::Dark.palette();
        let mut app = TuiApp::new("sess_fence", "w", false);
        app.messages.push(ChatMsg {
            role: "assistant".into(),
            text: "raciocinando\n```rust\nfn f() {}\n```".into(),
            kind: MsgKind::Reasoning,
        });
        let lines = history_lines(&app, &pal, 80, u16::MAX, false);
        assert!(lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains("THINK"))));
        // Corpo do código dentro da fence: bg do código E DIM/ITALIC.
        let codigo = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content == "fn f() {}"))
            .expect("linha de código presente");
        let span = codigo.spans.iter().find(|s| s.content == "fn f() {}").unwrap();
        assert_eq!(span.style.bg, Some(pal.code_bg));
        assert!(span.style.add_modifier.contains(Modifier::DIM));
        assert!(span.style.add_modifier.contains(Modifier::ITALIC));
        // A linha da fence também permanece DIM no reasoning.
        let fence = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content == "```rust"))
            .unwrap();
        let fspan = fence.spans.iter().find(|s| s.content == "```rust").unwrap();
        assert!(fspan.style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn markdown_rico_listas_bullets_e_numeradas() {
        use crate::ui::theme::Theme;
        let pal = Theme::Dark.palette();
        let msg = "- item **forte**\n* estrela\n  - sub\n1. um\n12. doze\ntexto normal";
        let rendered = markdown_message_spans(msg, &pal);
        assert_eq!(rendered.len(), 6, "1:1");
        let texto =
            |l: &[Span]| l.iter().map(|s| s.content.as_ref()).collect::<String>();
        // `- ` vira `• ` com fg muted; o resto da linha tem markdown normal.
        let l0 = &rendered[0];
        assert_eq!(texto(l0), "• item forte");
        let b = l0.iter().find(|s| s.content == "• ").unwrap();
        assert_eq!(b.style.fg, Some(pal.muted));
        assert!(l0.iter().any(
            |s| s.content == "forte" && s.style.add_modifier.contains(Modifier::BOLD)
        ));
        // A troca preserva a largura (2 células → 2 células).
        assert_eq!(cell_width("• "), cell_width("- "));
        // `* ` também é bullet.
        assert_eq!(texto(&rendered[1]), "• estrela");
        // Indentação de sub-item (2 espaços) preservada antes do bullet.
        let l2 = &rendered[2];
        assert_eq!(texto(l2), "  • sub");
        assert_eq!(l2[0].content, "  ");
        // Numerada mantém o número; só o marcador "1. "/"12. " é estilizado.
        let l3 = &rendered[3];
        assert_eq!(texto(l3), "1. um");
        assert_eq!(l3[0].style.fg, Some(pal.muted));
        let l4 = &rendered[4];
        assert_eq!(texto(l4), "12. doze");
        assert_eq!(l4[0].content, "12. ");
        assert_eq!(l4[0].style.fg, Some(pal.muted));
        assert_eq!(l4[1].content, "doze");
        // Linha comum sem marcador: intocada.
        assert_eq!(texto(&rendered[5]), "texto normal");
    }

    #[test]
    fn markdown_rico_titulos_h2_h3_com_fg_por_nivel() {
        use crate::ui::theme::Theme;
        let pal = Theme::Dark.palette();
        // `# ` original: BOLD puro, sem fg — inalterado.
        let h1 = markdown_spans_bg("# topo", None);
        assert!(h1[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(h1[0].style.fg, None);
        // `## ` e `### `: BOLD com fg de destaque distinto por nível.
        let h2 = markdown_message_spans("## meio", &pal);
        assert_eq!(h2[0][0].content, "meio");
        assert!(h2[0][0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(h2[0][0].style.fg, Some(pal.system));
        let h3 = markdown_message_spans("### raro", &pal);
        assert_eq!(h3[0][0].content, "raro");
        assert_eq!(h3[0][0].style.fg, Some(pal.badge_fg));
        // `##` sem espaço não é título (linha comum).
        let nao = markdown_message_spans("##sem espaço", &pal);
        assert!(nao[0].iter().any(|s| s.content.contains("##")));
    }

    #[test]
    fn patch_base_primeiro_preserva_fg_bg_do_markdown_no_papel() {
        use crate::ui::theme::Theme;
        let pal = Theme::Dark.palette();
        let mut app = TuiApp::new("sess_patch", "w", false);
        app.push_msg("user", "## plano\n**forte** `código`");
        let lines = history_lines(&app, &pal, 80, u16::MAX, false);
        let spans: Vec<_> = lines.iter().flat_map(|l| l.spans.iter()).collect();
        // FG explícito do markdown SOBREVIVE ao fg do papel (user): o título
        // `##` fica em system, NÃO em pal.user — mudança DELIBERADA desta
        // rodada (em V≤1 o patch invertido fazia o estilo do papel vencer).
        let titulo = spans.iter().find(|s| s.content == "plano").unwrap();
        assert_eq!(titulo.style.fg, Some(pal.system));
        assert!(titulo.style.add_modifier.contains(Modifier::BOLD));
        // Spans sem fg/bg próprio seguem idênticos ao patch antigo: bold
        // herda o fg do papel e inline code mantém bg do código + fg papel.
        let forte = spans.iter().find(|s| s.content == "forte").unwrap();
        assert!(forte.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(forte.style.fg, Some(pal.user));
        let codigo = spans.iter().find(|s| s.content == "código").unwrap();
        assert_eq!(codigo.style.bg, Some(pal.code_bg));
        assert_eq!(codigo.style.fg, Some(pal.user));
    }

    #[test]
    fn markdown_rico_transcript_1_para_1_e_medicao_estavel() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let pal = Theme::Dark.palette();
        let mut app = TuiApp::new("sess_rico", "w", false);
        app.push_msg(
            "assistant",
            "plano:\n- passo um\n- passo dois\n```bash\necho oi\n```\nfim",
        );
        let lines = history_lines(&app, &pal, 80, u16::MAX, false);
        // 7 linhas-fonte → 7 Lines (badge/indent prefixam; nada some ou soma).
        assert_eq!(lines.len(), 7);
        let texto = |l: &Line| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        assert_eq!(texto(&lines[0]), " ASSIST  plano:");
        assert_eq!(texto(&lines[1]), "   • passo um");
        assert_eq!(texto(&lines[4]), "   echo oi");
        // bg de código SÓ na linha interna da fence (o prefixo "   " é cru).
        assert_eq!(lines[4].spans.last().unwrap().style.bg, Some(pal.code_bg));
        assert!(lines[1].spans.iter().all(|s| s.style.bg.is_none()));
        assert!(lines[6].spans.iter().all(|s| s.style.bg.is_none()));
        // A medição de altura continua batendo com o render real do texto
        // construído (mesma propriedade de transcript_total_lines_*).
        let text = Text::from(lines);
        let largura = 12u16; // força wrap (badge+texto > 12 células)
        let medido = transcript_total_lines(&text, largura);
        let mut term = Terminal::new(TestBackend::new(largura, 40)).unwrap();
        term.draw(|f| {
            let par = Paragraph::new(text.clone()).wrap(Wrap { trim: false });
            f.render_widget(par, f.area());
        })
        .unwrap();
        let buf = term.backend().buffer();
        let renderizadas = (0..buf.area.height)
            .filter(|&y| (0..buf.area.width).any(|x| buf[(x, y)].symbol() != " "))
            .count();
        assert_eq!(medido as usize, renderizadas);
    }

    #[test]
    fn ctrlc_duplo_sai() {
        let mut a = TuiApp::new("s", "w", false);
        assert!(!a.ctrlc());
        assert!(!a.quit);
        a.disarm_ctrlc();
        assert!(!a.ctrlc());
        assert!(a.ctrlc());
        assert!(a.quit);
    }

    #[test]
    fn render_retro_largura_pequena_nao_quebra() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        for theme in [Theme::Dark, Theme::Light, Theme::Retro] {
            let pal = theme.palette();
            let mut app = TuiApp::new("sess_abcdef123456", "w", false);
            app.mode = "yolo".to_string();
            app.push_msg("user", "oi **forte**");
            app.push_msg("assistant", "ok `code`");
            app.perm_stub = Some(stub_permission());
            let backend = TestBackend::new(20, 12);
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|f| render(f, &mut app, &pal)).unwrap();
        }
    }

    #[test]
    fn confirm_consome_pendente() {
        let mut a = TuiApp::new("s", "w", false);
        assert_eq!(a.confirm_pending(true), None);
        a.pending_compact = true;
        assert_eq!(a.confirm_pending(false).unwrap(), "compact:no");
        assert!(!a.pending_compact);
        a.perm_stub = Some(stub_permission());
        let d = a.confirm_pending(true).unwrap();
        assert!(d.contains("stub"));
        assert!(a.perm_stub.is_none());
    }

    #[test]
    fn spinner_gira_10_frames() {
        let frames: Vec<char> = (0..10).map(spinner_frame).collect();
        assert_eq!(
            frames
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            10
        );
        assert_eq!(spinner_frame(10), spinner_frame(0));
        assert_eq!(spinner_frame(25), spinner_frame(5));
    }

    #[test]
    fn sidebar_telemetria_usa_contexto_tokens_e_velocidade() {
        let mut app = TuiApp::new("sess_sidebar", "C:/workspace/telemetry", false);
        app.set_usage(1_000_000, 2_000_000);
        app.ctx = crate::session::ContextUsage {
            window: 1_000_000,
            used: 0,
        };
        app.last_stats = Some(TurnStats {
            tok_per_s: 42.5,
            ..Default::default()
        });

        assert_eq!(compact_context_text(&app.ctx), "0.00M/1.00M (0%)");
        let compact_k = crate::session::ContextUsage {
            window: 1_000,
            used: 250,
        };
        assert_eq!(compact_context_text(&compact_k), "0.25K/1.00K (25%)");
        let telemetry = sidebar_telemetry_text(&app, 26);
        assert!(telemetry.contains("workspace"));
        assert!(telemetry
            .lines()
            .next()
            .is_some_and(|line| line.starts_with("workspace ")));
        assert!(telemetry.contains("context"));
        assert!(telemetry.contains("0.00M/1.00M"));
        assert!(telemetry.contains("tokens"));
        assert!(telemetry.contains("total 3.00M"));
        assert!(telemetry.contains("input 1.00M"));
        assert!(telemetry.contains("output 2.00M"));
        assert!(telemetry.contains("42.5 tok/s"));
        assert!(!telemetry.lines().any(|line| line == "status"));
        assert!(!telemetry.lines().any(|line| line == "pronto"));
    }

    #[test]
    fn pecas_visuais_fixtures() {
        assert_eq!(short_session("sess_abff123456789"), "sess_abff123…");
        assert_eq!(short_session("curta"), "curta");
        assert_eq!(role_badge("user"), "USER");
        assert_eq!(role_badge("assistant"), "ASSIST");
        assert_eq!(role_badge("system"), "SYS");
        assert!(help_bar().contains("Ctrl+J"));
        assert!(help_bar().contains("/compact"));
        assert!(help_bar().contains("Ctrl+C 2x"));
        let spans = markdown_spans_bg("x `code` y", Some(Color::Indexed(233)));
        let code = spans.iter().find(|s| s.content == "code").unwrap();
        assert_eq!(code.style.bg, Some(Color::Indexed(233)));
    }

    #[test]
    fn largura_visual_respeita_graphemes() {
        assert_eq!(cell_width("abc"), 3);
        assert_eq!(cell_width("界"), 2);
        assert_eq!(cell_width("e\u{301}"), 1);
        assert_eq!(cell_width("😀"), 2);
        assert!(cell_width(&truncate_cells("界😀abcdef", 5)) <= 5);
    }

    #[test]
    fn sidebar_olho_tem_fundo_nao_padrao_no_testbackend() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        // ERR-001: a sidebar agora tem painel ESTRUTURAL (olho half-block com
        // fg+bg), não um emblema ASCII recolorido. O fundo magenta DOMINA.
        let pal = Theme::Dark.palette();
        let mut app = TuiApp::new("sess_olho", "C:/ws", false);
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        let buffer = term.backend().buffer();
        let sidebar = sidebar_width_for(100) as usize; // 28 em largura 100
                                                       // Painel do olho: borda na linha 2, 12 linhas de arte (y 3..=14).
        let art_cells: Vec<_> = buffer
            .content()
            .chunks(100)
            .skip(3)
            .take(12)
            .flat_map(|row| {
                row.iter().take(sidebar).skip(1).take(sidebar - 2) // ignora as duas bordas verticais
            })
            .collect();
        assert_eq!(art_cells.len(), 12 * (sidebar - 2));
        assert!(
            art_cells.iter().all(|cell| cell.symbol() == "▀"),
            "painel do olho deve ser todo glifos half-block"
        );
        assert!(
            art_cells
                .iter()
                .filter(|cell| cell.bg != Color::Black && cell.bg != Color::Reset)
                .count()
                > 200,
            "fundo da arte (magenta) deve pintar as células"
        );
        assert!(
            art_cells
                .iter()
                .all(|cell| !cell.modifier.contains(Modifier::BOLD)),
            "arte não usa modificadores"
        );
        // Telemetria segue abaixo do painel do olho.
        let rows: Vec<String> = buffer
            .content()
            .chunks(100)
            .map(|row| row.iter().take(sidebar).map(|cell| cell.symbol()).collect())
            .collect();
        let sidebar_text = rows.join("\n");
        assert!(sidebar_text.contains("workspace"));
        assert!(sidebar_text.contains("context"));
        // Degradação graciosa: com sidebar de 9 linhas, o painel do olho some
        // (sem panic) e a telemetria ocupa tudo.
        let mut term_pequeno = Terminal::new(TestBackend::new(100, 15)).unwrap();
        term_pequeno.draw(|f| render(f, &mut app, &pal)).unwrap();
        let tela_pequena: String = term_pequeno
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(!tela_pequena.contains('▀'), "olho escondido em tela baixa");
        assert!(tela_pequena.contains("workspace"));
    }

    #[test]
    fn layout_mode_tem_breakpoints_deterministicos() {
        // A arte half-block tem seus breakpoints testados em `ui::art`
        // (dimensões por largura, recorte, 256 cores); aqui ficam só os
        // breakpoints de layout, que não dependem de assets.
        assert_eq!(layout_mode(Rect::new(0, 0, 80, 14), true), LayoutMode::Full);
        assert_eq!(
            layout_mode(Rect::new(0, 0, 80, 13), true),
            LayoutMode::Compact
        );
        assert_eq!(
            layout_mode(Rect::new(0, 0, 80, 12), true),
            LayoutMode::Compact
        );
        assert_eq!(
            layout_mode(Rect::new(0, 0, 80, 7), true),
            LayoutMode::Compact
        );
        assert_eq!(
            layout_mode(Rect::new(0, 0, 80, 6), true),
            LayoutMode::Compact
        );
        assert_eq!(
            layout_mode(Rect::new(0, 0, 80, 9), false),
            LayoutMode::Compact
        );
        assert_eq!(
            layout_mode(Rect::new(0, 0, 80, 8), false),
            LayoutMode::Compact
        );
        assert_eq!(
            layout_mode(Rect::new(0, 0, 80, 7), false),
            LayoutMode::Compact
        );
        assert_eq!(
            layout_mode(Rect::new(0, 0, 100, 24), false),
            LayoutMode::Full
        );
        assert_eq!(
            layout_mode(Rect::new(0, 0, 50, 24), false),
            LayoutMode::Compact
        );
    }

    #[test]
    fn primeira_tela_tem_arte_e_notices_sem_sys() {
        use crate::ui::render;
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};

        let mut app = TuiApp::new("sess_boot", "w", false);
        app.push_msg("system", &render::perm_unsupported_note());
        app.push_msg("system", render::splash(60));
        app.push_msg("user", "pergunta");
        app.push_msg("assistant", "resposta");

        let lines = history_lines(&app, &Theme::Dark.palette(), 80, u16::MAX, true);
        let flattened: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(!flattened.contains(" SYS "));
        assert!(flattened.contains("permissões:"));
        // Splash = grade half-block: 36 linhas × 76 colunas (largura 80 − 4),
        // cada span com fg E bg truecolor, fundo magenta dominante presente.
        let art_spans: Vec<_> = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.content.contains('▀'))
            .collect();
        let art_cells: usize = art_spans
            .iter()
            .map(|span| span.content.chars().count())
            .sum();
        assert_eq!(art_cells, 36 * 76);
        // Todo span da arte tem fg E bg (half-block exige os dois) — vale
        // tanto no modo truecolor quanto no degradado para 256 cores.
        assert!(art_spans
            .iter()
            .all(|span| span.style.fg.is_some() && span.style.bg.is_some()));
        // Cores exatas, de forma determinística (imune ao TERM do ambiente):
        // a arte truecolor tem o fundo magenta dominante (#FF65FF).
        let truecolor_art = art::art_text_styled(80, 36, false);
        let truecolor_spans: Vec<_> = truecolor_art
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .collect();
        assert!(truecolor_spans
            .iter()
            .all(|span| matches!(span.style.fg, Some(Color::Rgb(..)))
                && matches!(span.style.bg, Some(Color::Rgb(..)))));
        assert!(truecolor_spans
            .iter()
            .any(|span| span.style.bg == Some(Color::Rgb(255, 101, 255))));
        // A splash não é mais centralizada: ocupa a largura amostrada.
        assert!(lines
            .iter()
            .all(|line| line.alignment != Some(ratatui::layout::Alignment::Center)));
        let separators = lines
            .iter()
            .filter(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.chars().all(|ch| ch == '─'))
            })
            .count();
        assert_eq!(separators, 1, "only user/assistant should be separated");

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut app, &Theme::Dark.palette()))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(!screen.contains(" SYS "));
        assert!(screen.contains("permissões:"));
        // A arte (recorte central p/ a viewport) chega à tela.
        assert!(screen.contains('▀'));
    }

    #[test]
    fn dashboard_full_tem_regioes_em_colunas() {
        use crate::ui::render;
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};

        let mut app = TuiApp::new("sess_dashboard", "C:/workspace/demo", false);
        app.model = "zai/glm-5.3-Flash".into();
        app.mode = "operate".into();
        app.ctx = crate::session::ContextUsage {
            window: 1000,
            used: 250,
        };
        app.status = "pronto".into();
        app.push_msg("system", render::splash(60));
        app.push_msg("user", "pergunta no transcript");
        app.push_msg("assistant", "resposta no transcript");
        app.input = "prompt ativo".into();
        app.cursor = app.input_len();

        let palette = Theme::Dark.palette();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut app, &palette))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        let rows: Vec<String> = buffer
            .content()
            .chunks(100)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();

        assert!(rows.iter().any(|row| row.contains("zcode-cli")));
        assert!(rows.iter().any(|row| row.contains("transcript")));
        assert!(rows.iter().any(|row| row.contains("input:")));
        assert!(rows.iter().any(|row| row.contains("Enter")));
        assert!(screen.contains("pergunta no transcript"));
        assert!(screen.contains("prompt ativo"));
        // Telemetria segue na sidebar (abaixo do painel do olho); em 24 linhas
        // só as duas primeiras linhas cabem (as demais são afirmadas mais
        // abaixo, em terminal mais alto).
        assert!(screen.contains("workspace"));
        assert!(screen.contains("context"));
        assert!(screen.contains("$ "));
        // Nenhuma célula fica com bg padrão: o chrome pinta de preto e a arte
        // pinta as células dela (fg+bg half-block).
        assert!(buffer.content().iter().all(|cell| cell.bg != Color::Reset));

        // Painel do olho na sidebar: 12 linhas de ▀ com fundo não-preto.
        let sidebar = sidebar_width_for(100) as usize;
        assert!(100 - sidebar > sidebar, "transcript must be wider");
        let art_cells: Vec<_> = buffer
            .content()
            .chunks(100)
            .skip(3)
            .take(12)
            .flat_map(|row| {
                row.iter().take(sidebar).skip(1).take(sidebar - 2) // ignora as duas bordas verticais
            })
            .collect();
        assert!(art_cells.iter().all(|cell| cell.symbol() == "▀"));
        assert!(
            art_cells
                .iter()
                .filter(|cell| cell.bg != Color::Black)
                .count()
                > 200,
            "fundo magenta do olho deve dominar o painel"
        );
        // O emblema ASCII antigo (ZCODE/WORKSPACE/TERMINAL) foi REMOVIDO
        // (ERR-001: mudança estrutural, não só de cores).
        assert!(!screen.contains("ZCODE"));
        assert!(!screen.contains("TERMINAL"));

        // Terminal mais alto: telemetria completa sob o painel do olho.
        terminal.backend_mut().resize(100, 30);
        terminal
            .draw(|frame| render(frame, &mut app, &palette))
            .unwrap();
        let tela_alta: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(tela_alta.contains("tokens total"));
        assert!(tela_alta.contains("input"));
        assert!(tela_alta.contains("output"));
        assert!(tela_alta.contains("tok/s"));
        assert!(tela_alta.contains('▀'), "olho segue visível em telas altas");
    }

    #[test]
    fn dashboard_80_e_compacto_colapsam_deterministicamente() {
        use crate::ui::render;
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};

        let mut app = TuiApp::new("sess_compact", "C:/workspace/compact", false);
        app.model = "zai/glm-5.3-Flash".into();
        app.mode = "operate".into();
        app.push_msg("system", render::splash(60));
        app.push_msg("user", "historico preservado");
        app.input = "input preservado".into();
        app.cursor = app.input_len();
        let palette = Theme::Dark.palette();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut app, &palette))
            .unwrap();
        let screen_80: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen_80.contains("transcript"));
        assert!(screen_80.contains("historico preservado"));
        // Painel do olho visível na sidebar (half-block) e no transcript.
        assert!(screen_80.contains('▀'));
        assert!(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .all(|cell| cell.bg != Color::Reset));

        terminal.backend_mut().resize(50, 10);
        terminal
            .draw(|frame| render(frame, &mut app, &palette))
            .unwrap();
        let screen_compact: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        // Layout compacto: sem sidebar (sem olho, sem telemetria) e a splash
        // nem entra no transcript — só a conversa permanece.
        assert!(!screen_compact.contains('▀'));
        assert!(!screen_compact.contains("workspace / context"));
        assert!(screen_compact.contains("transcript"));
        assert!(screen_compact.contains("historico preservado"));
        assert!(screen_compact.contains("input preservado"));
        assert!(screen_compact.contains("$ "));
        assert!(screen_compact.contains("Enter"));
        assert!(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .all(|cell| cell.bg != Color::Reset));
    }

    #[test]
    fn help_e_cursor_cabem_na_area() {
        let mut app = TuiApp::new("s", "w", false);
        app.input = "界😀e\u{301}".into();
        app.cursor = app.input_len();
        assert_eq!(input_cursor_cell(&app), 5);
        for width in [0, 1, 20, 29, 39, 59, 80, 114] {
            assert!(cell_width(&help_bar_for_width(width)) <= width);
            assert!(cell_width(&confirmation_help_for_width(width)) <= width);
        }
        for token in ["Enter", "Ctrl+J", "/compact", "Esc", "Ctrl+C"] {
            assert!(help_bar_for_width(80).contains(token), "missing {token}");
        }
        assert_eq!(help_bar_for_width(20), "E|^J|/compact|Esc|^C");
    }

    #[test]
    fn resize_matrix_nao_quebra_e_preserva_estado() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let pal = Theme::Dark.palette();
        let mut app = TuiApp::new("sess_resize", "w", false);
        app.push_msg("system", crate::ui::art::SPLASH_MARKER);
        app.push_msg("user", "mensagem longa com 界 e emoji 😀");
        app.input = "linha 1\nlinha 2 界😀".into();
        app.cursor = app.input_len();
        app.working = true;
        app.perm_stub = Some(stub_permission());
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        for (width, height) in [
            (80, 24),
            (20, 12),
            (120, 40),
            (59, 13),
            (66, 14),
            (29, 7),
            (39, 9),
            // Full mode com sidebar curta: painel do olho é escondido.
            (80, 15),
            (80, 17),
        ] {
            term.backend_mut().resize(width, height);
            term.draw(|f| render(f, &mut app, &pal)).unwrap();
        }
    }

    #[test]
    fn transcript_total_lines_e_clamp_scroll_fixtures() {
        let text = Text::from(vec![
            Line::from("curta"),
            Line::from("a".repeat(25)),
            Line::from(""), // linha vazia conta 1
        ]);
        assert_eq!(transcript_total_lines(&text, 10), 1 + 3 + 1);
        assert_eq!(transcript_total_lines(&text, 25), 3); // 1 + 1 + 1 (nada quebra)
        assert_eq!(transcript_total_lines(&text, 0), 0); // área zero: não divide por zero
                                                         // CJK ocupa 2 células: 6 células em largura 4 → 2 linhas.
        assert_eq!(transcript_total_lines(&Text::from("界界界"), 4), 2);
        // Word-wrap real (quebra por palavra): palavras curtas de 3 células
        // em largura 10 — "aaa bbb" cabe (7), o espaço + "ccc" (11) estoura.
        // A estimativa antiga por largura bruta daria 27/10 → 3; a quebra do
        // Paragraph produz 4 linhas visuais.
        let texto = Text::from("aaa bbb ccc ddd eee fff ggg"); // 27 colunas brutas
        assert_eq!(transcript_total_lines(&texto, 10), 4);
        // clamp: offset nunca passa do fundo (total − inner_height).
        assert_eq!(clamp_scroll(0, 100, 10), 0);
        assert_eq!(clamp_scroll(50, 100, 10), 50);
        assert_eq!(clamp_scroll(95, 100, 10), 90);
        assert_eq!(clamp_scroll(200, 100, 10), 90);
        assert_eq!(clamp_scroll(5, 3, 10), 0); // conteúdo menor que a viewport
    }

    /// Verdade de referência: renderiza um `Paragraph` com a MESMA
    /// configuração do transcript (wrap `trim = false`, sem block) em um
    /// `TestBackend` e conta as linhas visuais ocupadas (índice da última
    /// célula não-espaço + 1). `altura` precisa ser um limitante superior do
    /// wrap (usado só p/ dimensionar o backend).
    fn linhas_visuais_renderizadas(text: &Text, largura: u16, altura: u16) -> usize {
        use ratatui::{backend::TestBackend, Terminal};
        let mut term = Terminal::new(TestBackend::new(largura.max(1), altura)).unwrap();
        term.draw(|f| {
            let par = Paragraph::new(text.clone()).wrap(Wrap { trim: false });
            f.render_widget(par, f.area());
        })
        .unwrap();
        let buf = term.backend().buffer();
        let mut ocupadas = 0usize;
        for y in 0..buf.area.height as usize {
            for x in 0..buf.area.width as usize {
                if buf[(x as u16, y as u16)].symbol() != " " {
                    ocupadas = ocupadas.max(y + 1);
                    break;
                }
            }
        }
        ocupadas
    }

    #[test]
    fn transcript_total_lines_bate_com_paragraph_word_wrap() {
        // Propriedade: a medição de transcript_total_lines é IGUAL ao que o
        // Paragraph realmente quebra, para textos variados (PRNG xorshift
        // determinístico — sem dependências novas) e larguras arbitrárias.
        let mut st = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            st
        };
        for _ in 0..100 {
            // 1..=3 linhas-fonte, 1..=12 palavras de 1..=6 células; sem
            // espaço no fim (a última linha visual nunca é branca).
            let texto: String = (0..1 + next() % 3)
                .map(|_| {
                    (0..1 + next() % 12)
                        .map(|_| {
                            let n = 1 + (next() % 6) as usize;
                            "abcdef".chars().take(n).collect::<String>()
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect::<Vec<_>>()
                .join("\n");
            let text = Text::from(texto);
            let largura = 1 + (next() % 40) as u16;
            // Limitante superior p/ o backend: cada linha-fonte rende no
            // máximo (nº de graphemes + 1) linhas visuais (trim=false).
            let altura: u16 = text
                .lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.chars().count() as u16)
                        .sum::<u16>()
                        + 1
                })
                .sum::<u16>()
                .max(1);
            assert_eq!(
                transcript_total_lines(&text, largura) as usize,
                linhas_visuais_renderizadas(&text, largura, altura),
                "diverge do render real p/ {:?} @ largura {largura}",
                text.lines
                    .iter()
                    .map(|l| l
                        .spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>())
                    .collect::<Vec<_>>()
            );
        }
        // Bordas: vazio e largura 1 não panican e são consistentes.
        assert_eq!(transcript_total_lines(&Text::from(""), 1), 1);
        assert_eq!(
            transcript_total_lines(&Text::from("ab"), 1) as usize,
            linhas_visuais_renderizadas(&Text::from("ab"), 1, 4)
        );
    }

    #[test]
    fn page_step_usa_altura_medida_com_fallback() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let mut a = TuiApp::new("s", "w", false);
        // Antes do 1º render (0) → passo fixo de 10.
        assert_eq!(a.page_step(), 10);
        // Render real grava a altura interna do transcript (write-back).
        let pal = Theme::Dark.palette();
        a.push_msg("assistant", "linha");
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| render(f, &mut a, &pal)).unwrap();
        let h = a.last_inner_height;
        assert!(h > 0, "render grava a viewport real do transcript");
        assert_eq!(
            a.page_step(),
            h.saturating_sub(2).max(1),
            "passo = altura - 2"
        );
        // Viewport pequena (1..=2): mínimo 1 linha.
        a.last_inner_height = 2;
        assert_eq!(a.page_step(), 1);
        a.last_inner_height = 1;
        assert_eq!(a.page_step(), 1);
        // Área zero de volta ao fallback.
        a.last_inner_height = 0;
        assert_eq!(a.page_step(), 10);
    }

    #[test]
    fn push_msg_preserva_follow_e_nao_reseta_scroll() {
        let mut a = TuiApp::new("s", "w", false);
        assert!(a.follow, "follow começa true");
        a.scroll = 42;
        a.push_msg("user", "oi");
        assert_eq!(a.scroll, 42, "push não zera o offset");
        assert!(a.follow);
        a.follow = false;
        a.push_msg("assistant", "olá");
        assert!(!a.follow, "push não re-engaja follow sozinho");
        assert_eq!(a.scroll, 42);
    }

    #[test]
    fn scroll_metodos_transicao_follow() {
        let mut a = TuiApp::new("s", "w", false);
        // O render grava o offset do fundo em `scroll` (follow); simule:
        a.scroll = 90;
        a.scroll_up(10);
        assert!(!a.follow);
        assert_eq!(a.scroll, 80);
        a.scroll_down(5);
        assert_eq!(a.scroll, 85);
        a.scroll_home();
        assert!(!a.follow);
        assert_eq!(a.scroll, 0);
        a.scroll_end();
        assert!(a.follow);
        // follow → scroll_down é no-op (já no fim).
        a.scroll = 90;
        a.scroll_down(5);
        assert_eq!(a.scroll, 90);
        // saturando no topo.
        a.scroll_home();
        a.scroll_up(7);
        assert_eq!(a.scroll, 0);
    }

    #[test]
    fn render_transcript_follow_clamp_e_religamento() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let pal = Theme::Dark.palette();
        let mut app = TuiApp::new("sess_scroll", "w", false);
        // 40 linhas curtas garantem overflow na viewport de 100x24.
        let corpo: String = (0..40)
            .map(|i| format!("linha {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.push_msg("assistant", &corpo);
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        // 1º draw: follow gruda no fim e grava o offset real em `scroll`.
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        assert!(app.follow);
        let bottom = app.scroll;
        assert!(
            bottom > 0,
            "conteúdo maior que a viewport → offset de fundo > 0"
        );
        // PageUp: sai do follow e sobe de verdade (não fica preso no fundo).
        app.scroll_up(5);
        assert!(!app.follow);
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        assert_eq!(app.scroll, bottom.saturating_sub(5));
        // Scroll até o fundo: follow re-engaja e o offset volta ao máximo.
        app.scroll_down(1_000);
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        assert!(
            app.follow,
            "chegar ao fundo pelo scroll_down religa o follow"
        );
        assert_eq!(app.scroll, bottom);
    }

    #[test]
    fn mapa_de_teclas_scroll_transcript() {
        use KeyCode as K;
        use KeyModifiers as M;
        assert_eq!(map_key(key(K::Home, M::empty())), TuiKey::Home);
        assert_eq!(map_key(key(K::End, M::empty())), TuiKey::End);
        assert_eq!(map_key(key(K::Home, M::CONTROL)), TuiKey::CtrlHome);
        assert_eq!(map_key(key(K::End, M::CONTROL)), TuiKey::CtrlEnd);
    }

    // ----- cache do transcript (fluidez: sem re-parse por draw) -----

    #[test]
    fn transcript_cache_key_e_rev_largura_altura() {
        use crate::ui::theme::Theme;
        let pal = Theme::Dark.palette();
        let mut app = TuiApp::new("sess_cache", "w", false);
        app.push_msg("user", "oi **forte**");
        let _ = transcript_text(&mut app, &pal, 60, 10);
        let key = (app.messages_rev, 60u16, 10u16);
        assert!(
            app.transcript_cache
                .as_ref()
                .is_some_and(|c| (c.0, c.1, c.2) == key),
            "1º build popula o cache com a chave (rev, w, h)"
        );
        // Mesma chave → cache intacto (hit, sem rebuild).
        let antes = app.transcript_cache.as_ref().unwrap().3.clone();
        let _ = transcript_text(&mut app, &pal, 60, 10);
        assert_eq!(app.transcript_cache.as_ref().unwrap().3, antes);
        // Largura nova invalida.
        let _ = transcript_text(&mut app, &pal, 40, 10);
        assert_eq!(app.transcript_cache.as_ref().unwrap().1, 40);
        // Altura nova invalida (splash recorta por altura da viewport).
        let _ = transcript_text(&mut app, &pal, 40, 20);
        assert_eq!(app.transcript_cache.as_ref().unwrap().2, 20);
        // push_msg bumpa a versão → próxima build troca a chave.
        let rev = app.messages_rev;
        app.push_msg("assistant", "olá");
        assert_eq!(app.messages_rev, rev + 1, "push_msg bumpa messages_rev");
        let _ = transcript_text(&mut app, &pal, 40, 20);
        assert_eq!(app.transcript_cache.as_ref().unwrap().0, app.messages_rev);
        // bump_rev manual (clear/replace) também invalida: a chave armazenada
        // não bate mais com a versão atual → próximo render reconstrói.
        app.bump_rev();
        let atual = (app.messages_rev, 40u16, 20u16);
        assert!(!app
            .transcript_cache
            .as_ref()
            .is_some_and(|c| (c.0, c.1, c.2) == atual));
    }

    #[test]
    fn transcript_spinner_por_frame_nao_invalida_cache() {
        use crate::ui::theme::Theme;
        let pal = Theme::Dark.palette();
        let mut app = TuiApp::new("sess_spin", "w", false);
        app.push_msg("user", "oi");
        let _ = transcript_text(&mut app, &pal, 50, 8);
        let cached = app.transcript_cache.as_ref().unwrap().3.clone();
        // working + tick (spinner animando) NÃO entram no cache: o texto
        // em cache fica estável enquanto o turno roda.
        app.working = true;
        app.tick = 3;
        let t = transcript_text(&mut app, &pal, 50, 8);
        assert_eq!(
            app.transcript_cache.as_ref().unwrap().3,
            cached,
            "spinner não é cacheado"
        );
        assert_eq!(t.lines.len(), cached.len() + 1, "+1 linha working");
        assert!(t
            .lines
            .last()
            .unwrap()
            .spans
            .iter()
            .any(|s| s.content.contains("working…")));
    }

    #[test]
    fn reasoning_render_think_dim_e_prefixo_igual() {
        use crate::ui::theme::Theme;
        let pal = Theme::Dark.palette();
        let mut app = TuiApp::new("sess_think", "w", false);
        app.push_msg("assistant", "resposta normal");
        // Mensagem reasoning (como o poller injeta via MsgItem).
        app.messages.push(ChatMsg {
            role: "assistant".into(),
            text: "pensando muito sobre o problema".into(),
            kind: MsgKind::Reasoning,
        });
        let lines = history_lines(&app, &pal, 80, u16::MAX, false);
        let think: Vec<_> = lines
            .iter()
            .filter(|l| l.spans.iter().any(|s| s.content.contains("THINK")))
            .collect();
        assert_eq!(think.len(), 1, "badge THINK no lugar do ASSIST");
        // Estilo DIM (+ itálico) e cor muted no corpo do reasoning.
        let corpo = think[0]
            .spans
            .iter()
            .find(|s| s.content.contains("pensando"))
            .expect("corpo do reasoning presente");
        assert!(corpo.style.add_modifier.contains(Modifier::DIM));
        assert!(corpo.style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(corpo.style.fg, Some(pal.muted));
        // Geometria: prefixo do reasoning com a MESMA largura de células do
        // assistant (wrap das continuation lines não desalinha).
        let largura = |l: &Line| l.spans[0].content.chars().count();
        let assist_idx = lines
            .iter()
            .position(|l| l.spans.iter().any(|s| s.content.contains("ASSIST")))
            .unwrap();
        assert_eq!(largura(&lines[assist_idx]), largura(think[0]));
        assert_eq!(largura(think[0]), 8, "\" ASSIST \" e \" THINK  \" = 8 células");
        // Texto normal continua SEM DIM.
        let normal = lines[assist_idx]
            .spans
            .iter()
            .find(|s| s.content.contains("resposta"))
            .unwrap();
        assert!(!normal.style.add_modifier.contains(Modifier::DIM));
        assert!(badge_label("assistant", MsgKind::Reasoning) == "THINK");
        assert!(badge_label("assistant", MsgKind::Text) == "ASSIST");
    }

    #[test]
    fn header_dead_estado_e_cor_no_testbackend() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let pal = Theme::Dark.palette();
        let mut app = TuiApp::new("sess_dead", "w", false);
        app.push_msg("user", "oi");
        app.runtime_dead = true;
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        let rows: Vec<String> = term
            .backend()
            .buffer()
            .content()
            .chunks(100)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        assert!(rows.iter().any(|r| r.contains("state=DEAD")), "estado DEAD no header");
        // Header inteiro na cor de erro da paleta (system) + BOLD.
        let celula = &term.backend().buffer()[(0, 0)];
        assert_eq!(celula.fg, pal.system);
        assert!(celula.modifier.contains(Modifier::BOLD));
        // Sem runtime_dead volta a ready/assistant.
        app.runtime_dead = false;
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        let rows2: Vec<String> = term
            .backend()
            .buffer()
            .content()
            .chunks(100)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        assert!(rows2.iter().any(|r| r.contains("state=ready")));
        assert_eq!(term.backend().buffer()[(0, 0)].fg, pal.assistant);
    }

    // ----- paste (insert_str) e histórico de prompts (↑/↓) -----

    #[test]
    fn insert_str_multibyte_e_quebras_normalizadas() {
        let mut a = TuiApp::new("s", "w", false);
        a.insert_str("olá 😀");
        assert_eq!(a.input, "olá 😀");
        assert_eq!(a.cursor, a.input_len(), "cursor avança em CHARs, não bytes");
        // Inserção no MEIO (antes do emoji) fica em fronteira de char.
        a.cursor = "olá ".chars().count();
        a.insert_str("界");
        assert_eq!(a.input, "olá 界😀");
        assert_eq!(a.cursor, "olá 界".chars().count());
        // Quebras do clipboard saneadas (`\r\n`/`\r` → `\n`) e ficam NO buffer.
        let mut b = TuiApp::new("s", "w", false);
        b.insert_str("l1\r\nl2\rl3");
        assert_eq!(b.input, "l1\nl2\nl3");
        assert_eq!(b.cursor, b.input_len());
        // Paste vazio é no-op.
        let mut c = TuiApp::new("s", "w", false);
        c.insert_str("");
        assert_eq!(c.input, "");
        assert_eq!(c.cursor, 0);
    }

    #[test]
    fn historico_remember_dedupe_consecutivo() {
        let mut a = TuiApp::new("s", "w", false);
        a.remember_prompt("oi");
        a.remember_prompt("oi");
        assert_eq!(a.prompt_history, vec!["oi".to_string()], "igual consecutivo não duplica");
        a.remember_prompt("olá");
        a.remember_prompt("oi");
        assert_eq!(a.prompt_history, vec!["oi".to_string(), "olá".into(), "oi".into()]);
        // Lembrar encerra qualquer navegação em curso (e descarta o draft).
        a.up_key();
        assert!(a.prompt_history_idx.is_some());
        a.remember_prompt("x");
        assert!(a.prompt_history_idx.is_none());
        assert!(a.draft.is_none());
    }

    #[test]
    fn historico_cap_100_descarta_mais_antigo() {
        let mut a = TuiApp::new("s", "w", false);
        for i in 0..(PROMPT_HISTORY_CAP + 20) {
            a.remember_prompt(&format!("p{i}"));
        }
        assert_eq!(a.prompt_history.len(), PROMPT_HISTORY_CAP);
        assert_eq!(a.prompt_history.first().unwrap(), "p20", "o mais antigo sai");
        assert_eq!(
            a.prompt_history.last().unwrap(),
            &format!("p{}", PROMPT_HISTORY_CAP + 19)
        );
    }

    #[test]
    fn historico_up_a_partir_de_vazio_e_single_line() {
        let mut a = TuiApp::new("s", "w", false);
        a.remember_prompt("primeiro");
        a.remember_prompt("segundo");
        // Buffer vazio: ↑ pega o mais novo, cursor no fim.
        a.up_key();
        assert_eq!(a.input, "segundo");
        assert_eq!(a.cursor, a.input_len());
        // ↑ de novo: item anterior; no mais antigo fica parado.
        a.up_key();
        assert_eq!(a.input, "primeiro");
        a.up_key();
        assert_eq!(a.input, "primeiro");
        // ↓ até o fim restaura o draft (buffer era vazio) e volta à edição livre.
        a.down_key();
        a.down_key();
        assert_eq!(a.input, "");
        assert!(a.prompt_history_idx.is_none());
        assert!(a.draft.is_none());
        // Sem navegação ativa, ↓ em single-line é a navegação de cursor (no-op).
        a.input = "abc".into();
        a.cursor = 1;
        a.down_key();
        assert_eq!(a.input, "abc");
        assert_eq!(a.cursor, 1);
    }

    #[test]
    fn historico_draft_preservado_e_restaurado() {
        let mut a = TuiApp::new("s", "w", false);
        a.remember_prompt("antigo");
        a.input = "rascunho em curso".into();
        a.cursor = 8; // no meio do rascunho
        a.up_key();
        assert_eq!(a.input, "antigo");
        assert_eq!(a.draft.as_deref(), Some("rascunho em curso"));
        assert_eq!(a.cursor, a.input_len(), "item do histórico com cursor no fim");
        // ↓ além do mais novo restaura o rascunho intacto.
        a.down_key();
        assert_eq!(a.input, "rascunho em curso");
        assert_eq!(a.cursor, a.input_len());
        assert!(a.prompt_history_idx.is_none());
        assert!(a.draft.is_none());
    }

    #[test]
    fn historico_multilinha_nao_navega_up_down() {
        let mut a = TuiApp::new("s", "w", false);
        a.remember_prompt("h1");
        a.insert_str("l1\nl2");
        a.cursor = 0; // cursor na 1ª linha — mesmo assim NÃO navega
        a.up_key();
        assert_eq!(a.input, "l1\nl2");
        assert!(a.prompt_history_idx.is_none());
        assert!(a.draft.is_none());
        // ↓ com cursor na última linha também mantém a navegação de cursor.
        a.cursor = a.input_len();
        a.down_key();
        assert_eq!(a.input, "l1\nl2");
        assert!(a.prompt_history_idx.is_none());
    }

    #[test]
    fn historico_edicao_sai_do_modo_navegacao() {
        let mut a = TuiApp::new("s", "w", false);
        a.remember_prompt("h1");
        a.up_key();
        assert!(a.prompt_history_idx.is_some());
        a.insert_char('!');
        assert!(a.prompt_history_idx.is_none() && a.draft.is_none());
        a.up_key();
        assert!(a.prompt_history_idx.is_some());
        a.backspace();
        assert!(a.prompt_history_idx.is_none());
    }

    #[test]
    fn ctrl_up_down_map_e_navegam_multilinha() {
        use KeyCode as K;
        use KeyModifiers as M;
        assert_eq!(map_key(key(K::Up, M::CONTROL)), TuiKey::CtrlUp);
        assert_eq!(map_key(key(K::Down, M::CONTROL)), TuiKey::CtrlDown);
        let mut a = TuiApp::new("s", "w", false);
        a.remember_prompt("h1");
        a.remember_prompt("h2");
        a.insert_str("l1\nl2"); // multilinha: ↑ normal NÃO navega...
        a.up_key();
        assert_eq!(a.input, "l1\nl2");
        // ...mas Ctrl+↑ substitui o buffer sempre (sem restrição de linha).
        a.history_up_force();
        assert_eq!(a.input, "h2");
        assert_eq!(a.cursor, a.input_len());
        // Ctrl+↓ devolve ao draft (o texto multilinha de antes).
        a.history_down_force();
        assert_eq!(a.input, "l1\nl2");
        assert!(a.prompt_history_idx.is_none() && a.draft.is_none());
        // Sem navegação ativa, Ctrl+↓ é no-op.
        a.history_down_force();
        assert_eq!(a.input, "l1\nl2");
    }
}
