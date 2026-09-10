//! TUI rica: estado puro e render ratatui.
//!
//! Shell de agente com header, corpo horizontal, sidebar, transcript,
//! prompt inferior e rodape. Streaming e permissoes reais seguem o
//! fluxo existente; esta camada altera somente a apresentacao.

use crate::session::{ContextUsage, MsgKind, SessionRow, TodoItem, TodoStatus, TurnStats, Usage};
use crate::ui::art;
use crate::ui::theme::{self, Palette};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::VecDeque;
use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
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
    /// Ctrl+F — abre a linha de busca no transcript (Fase V4-2).
    CtrlF,
    /// F3 — próximo match da busca (sticky ou linha aberta).
    F3,
    /// Shift+F3 — match anterior da busca.
    ShiftF3,
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
        (KeyCode::Char('f'), m) if m.contains(KeyModifiers::CONTROL) => TuiKey::CtrlF,
        // F3 com Shift = anterior; sem modificadores (ou só SHIFT em terminais
        // que não distinguem) = próximo. CONTROL/ALT ficam de fora (não são
        // busca).
        (KeyCode::F(3), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            if m.contains(KeyModifiers::SHIFT) {
                TuiKey::ShiftF3
            } else {
                TuiKey::F3
            }
        }
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
/// conteúdo (ignorando indentação) começa com ``` — INCLUSIVE fences de 4+
/// crases, que `starts_with` também casa (viram bloco cru com bg, sem
/// highlight; aceitável e documentado). Simples de propósito — não é um
/// parser CommonMark (fences `~~~` sim ficam como texto).
fn linha_fence(raw: &str) -> bool {
    raw.trim_start().starts_with("```")
}

/// Linguagem declarada na fence de abertura (` ```rust ignore ` → "rust").
/// `None` = fence nua (` ``` `): o corpo segue no caminho cru (comportamento
/// de sempre). O resto da marca (flags, título do bloco) é ignorado.
fn fence_lang(raw: &str) -> Option<String> {
    let conteudo = raw.trim_start().strip_prefix("```")?;
    let lang = conteudo.trim().split_whitespace().next().unwrap_or("");
    if lang.is_empty() {
        None
    } else {
        Some(lang.to_string())
    }
}

/// Slot semântico do highlight → Style da NOSSA paleta (Fase V5-2). Toda
/// variante mantém o bg `code_bg` (o bloco continua sendo um bloco); o fg
/// segue o papel da paleta ativa:
/// - Keyword → `prompt` (rosa/magenta no Dark — cor clássica de keyword);
/// - Str → `system` (âmbar no Dark, verde no Retro — "verde-ish" existente);
/// - Comment → `muted` + itálico (o mesmo tom de raciocínio);
/// - Const → `badge_fg` (ciano claro);
/// - Func → `user` (azul-ciano) e Type → `border` (roxo);
/// - Plain → sem fg (herda o papel, como o bloco cru de sempre).
/// No Retro todas as cores colapsam para a família verde (identidade
/// monocrômica preservada); na paleta 256 os índices equivalentes são usados
/// (cores NUNCA saem da paleta ativa — identidade do tema mantida).
fn slot_style(slot: crate::ui::highlight::TokenSlot, pal: &Palette) -> Style {
    use crate::ui::highlight::TokenSlot;
    let mut st = Style::default().bg(pal.code_bg);
    let fg = match slot {
        TokenSlot::Plain => None,
        TokenSlot::Keyword => Some(pal.prompt),
        TokenSlot::Str => Some(pal.system),
        TokenSlot::Comment => Some(pal.muted),
        TokenSlot::Const => Some(pal.badge_fg),
        TokenSlot::Func => Some(pal.user),
        TokenSlot::Type => Some(pal.border),
    };
    if let Some(fg) = fg {
        st = st.fg(fg);
    }
    if slot == TokenSlot::Comment {
        st = st.add_modifier(Modifier::ITALIC);
    }
    st
}

/// Pré-processamento markdown de UMA mensagem, LINHA a LINHA:
/// - blocos cercados por ``` (state machine "dentro de fence"): a linha da
///   fence é renderizada DIM/muted (a linguagem declarada em ```rust segue
///   visível, pois a própria fence cruza como texto) e as linhas internas
///   ganham bg = `code_bg` — o MESMO fundo do inline code — SEM processamento
///   de markdown, preservando espaços (o wrap continua por conta do
///   Paragraph). Com LINGUAGEM conhecida pelo syntect (Fase V5-2), o bloco
///   INTEIRO é highlightado (`ui::highlight`, cache LRU por (linguagem,
///   conteúdo)): cada linha vira spans com fg da NOSSA paleta por slot
///   semântico, preservando o bg do código. Qualquer falha do highlight →
///   caminho cru de sempre (o código nunca some);
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
    let linhas: Vec<&'a str> = text.lines().collect();
    let mut out: Vec<Vec<Span<'a>>> = Vec::with_capacity(linhas.len());
    let mut i = 0usize;
    while i < linhas.len() {
        let raw = linhas[i];
        if !linha_fence(raw) {
            out.push(linha_comum_spans(raw, pal));
            i += 1;
            continue;
        }
        // Abre fence: a marca é esmaecida (```rust segue legível como texto).
        out.push(vec![Span::styled(raw, fence_style)]);
        let inicio = i + 1;
        let mut fim = inicio;
        while fim < linhas.len() && !linha_fence(linhas[fim]) {
            fim += 1;
        }
        let corpo = &linhas[inicio..fim];
        // Highlight (Fase V5-2): fence COM linguagem conhecida → o bloco
        // inteiro é parseado de uma vez (estado multiline do syntect) e cada
        // linha vira spans (slot, texto) com estilo da paleta. Sem linguagem,
        // linguagem desconhecida ou qualquer falha (erro de parse/contrato
        // 1:1 furado) → texto cru com bg, EXATAMENTE como antes.
        if let Some(lang) = fence_lang(raw).filter(|l| {
            crate::ui::highlight::known_language(l)
        }) {
            let code = corpo.join("\n");
            match crate::ui::highlight::highlight_block(&lang, &code) {
                Some(blocos) => {
                    for spans in blocos {
                        out.push(
                            spans
                                .into_iter()
                                .map(|(slot, t)| Span::styled(t, slot_style(slot, pal)))
                                .collect(),
                        );
                    }
                }
                None => {
                    for l in corpo {
                        out.push(vec![Span::styled(*l, code_style)]);
                    }
                }
            }
        } else {
            for l in corpo {
                out.push(vec![Span::styled(*l, code_style)]);
            }
        }
        if fim < linhas.len() {
            // Fecha a fence: esmaece e retoma o markdown na linha seguinte.
            out.push(vec![Span::styled(linhas[fim], fence_style)]);
            i = fim + 1;
        } else {
            // Fence não fechada: o corpo vai até o fim da mensagem.
            i = fim;
        }
    }
    out
}

/// Linha COMUM do markdown (fora de fence): títulos/bullets/numeradas/inline.
/// Extraída de `markdown_message_spans` (comportamento idêntico ao de V≤4).
fn linha_comum_spans<'a>(raw: &'a str, pal: &Palette) -> Vec<Span<'a>> {
    let marker_style = Style::default().fg(pal.muted);
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

// ---------- busca no transcript (Ctrl+F, Fase V4-2) ----------

/// Um resultado da busca: posição da mensagem no histórico + preview curto.
/// `role` vem como badge curto (USER/ASSIST/THINK/SYS) p/ reuso no status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub msg_idx: usize,
    pub preview: String,
    pub role: String,
}

/// Estado da busca inline (linha "find:" no lugar do input, estilo less/vim).
/// - `open == true`: a linha está visível e captura o teclado (digitação);
/// - `open == false`: busca "sticky" pós-Enter — invisível, mas F3/Shift+F3
///   continuam navegando e Ctrl+F reabre com a query preservada;
/// - `saved_input`/`saved_cursor`: buffer do input guardado ao abrir — a
///   busca NUNCA destrói o texto do usuário (restaurado ao fechar, por
///   Enter ou por Esc);
/// - `qcursor`: cursor de edição da query em nº de chars (mesma unidade do
///   editor do input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchState {
    pub query: String,
    pub matches: Vec<SearchMatch>,
    /// Índice do match ATUAL (base 0).
    pub cursor: usize,
    pub qcursor: usize,
    pub open: bool,
    pub saved_input: String,
    pub saved_cursor: usize,
}

/// Busca linear case-insensitive no histórico (barata: roda a cada tecla).
/// Inclui TUDO exceto o marcador de splash (reasoning incluso — o usuário
/// busca o que o agente pensou; notices system entram como SYS). Query
/// vazia/em branco → nenhum resultado.
pub fn find_matches(messages: &[ChatMsg], query: &str) -> Vec<SearchMatch> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        if is_splash(&m.text) {
            continue;
        }
        if m.text.to_lowercase().contains(&q) {
            out.push(SearchMatch {
                msg_idx: i,
                preview: match_preview(&m.text, &q),
                role: badge_label(&m.role, m.kind).to_string(),
            });
        }
    }
    out
}

/// Preview do match: primeira linha da mensagem QUE CONTÉM a query (fallback:
/// 1ª linha não vazia), trimada e truncada em ~60 células.
fn match_preview(text: &str, q_lower: &str) -> String {
    let linha = text
        .lines()
        .find(|l| l.to_lowercase().contains(q_lower))
        .or_else(|| text.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or("");
    truncate_cells(linha.trim(), 60)
}

/// Offset de scroll p/ centralizar a linha do match na viewport: meia tela
/// acima do início da mensagem (saturando no topo). O clamp do teto
/// (`total − inner_height`) é do render (`clamp_scroll`), como todo scroll.
pub fn jump_offset(prefix_lines: u16, inner_height: u16) -> u16 {
    prefix_lines.saturating_sub(inner_height / 2)
}

/// Status curto da busca p/ a linha find: ("match 2/5") ou "sem resultados".
pub fn search_status(s: &SearchState) -> String {
    if s.matches.is_empty() {
        "sem resultados".to_string()
    } else {
        format!("match {}/{}", s.cursor + 1, s.matches.len())
    }
}

/// Byte da fronteira do cursor da query (mesma unidade em CHARs do editor
/// do input — nunca quebra multibyte).
fn qcursor_byte(s: &SearchState) -> usize {
    s.query
        .char_indices()
        .nth(s.qcursor)
        .map(|(b, _)| b)
        .unwrap_or(s.query.len())
}

// ---------- picker de sessões (/resume sem argumento, Fase V5-2) ----------

/// Estado dos dados do picker: busca em voo (o RPC `session/list` roda em
/// spawn, como todo RPC da TUI) / resultado vazio / lista pronta.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PickerData {
    #[default]
    Loading,
    Empty,
    Items(Vec<SessionRow>),
}

/// Picker modal de sessões (estilo Todos/Context, mas INTERATIVO: captura o
/// teclado até Enter/Esc/Ctrl+C — por isso NÃO é um `Overlay` do enum, que
/// fecha com qualquer tecla). Navegação ↑/↓ com CLAMP (sem wrap — nos
/// extremos fica parado, MESMA decisão da navegação de matches da busca:
/// "menos surpresa"); PageUp/PageDown em passos fixos de 10 (a altura real
/// da viewport não chega no handler de teclas).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionPicker {
    pub data: PickerData,
    /// Índice da linha selecionada (base 0); sempre clampado ao tamanho.
    pub selected: usize,
}

impl SessionPicker {
    pub fn loading() -> Self {
        Self::default()
    }

    /// Nº de itens prontos (0 em Loading/Empty).
    pub fn len(&self) -> usize {
        match &self.data {
            PickerData::Items(v) => v.len(),
            _ => 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Aplica o resultado do fetch (itens ou vazio) e volta a seleção ao
    /// primeiro item. Chamado pelo `apply_ui_update` (task da UI).
    pub fn fill(&mut self, rows: Vec<SessionRow>) {
        self.data = if rows.is_empty() {
            PickerData::Empty
        } else {
            PickerData::Items(rows)
        };
        self.selected = 0;
    }

    /// Move a seleção `delta` passos com clamp (↑ no topo fica; ↓ no fim
    /// fica). Lista vazia → no-op. `delta` pode ser qualquer magnitude
    /// (PageUp/PageDown usam passos de 10).
    pub fn move_selected(&mut self, delta: i32) {
        let n = self.len();
        if n == 0 {
            return;
        }
        let atual = self.selected as i32;
        let novo = (atual + delta).clamp(0, n as i32 - 1);
        self.selected = novo as usize;
    }

    /// Id da sessão selecionada (None em Loading/Empty).
    pub fn selected_id(&self) -> Option<String> {
        match &self.data {
            PickerData::Items(v) => v.get(self.selected).map(|r| r.id.clone()),
            _ => None,
        }
    }
}

// ---------- estado ----------

/// Overlay modal aberto sobre o layout normal (`/context`, `/usage`, `/todos`).
/// Fecha com QUALQUER tecla (consumida — nada entra no buffer; Esc não
/// cancela turno). Referência visual: painéis /context e /usage do Claude
/// Code (janela centrada, título + resumo à direita, barra e legenda).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Overlay {
    Context,
    Usage,
    /// `/todos`: checklist do agente (badges `[ ]`/`[~]`/`[x]` + contagem).
    Todos,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMsg {
    pub role: String,
    pub text: String,
    /// Reasoning vira mensagem própria (DIM + badge THINK) vinda do poller.
    pub kind: MsgKind,
    /// Texto de FIO (A-1): o que EFETIVAMENTE foi enviado ao servidor quando
    /// difere do `text` exibido (ex.: envio com `@arquivo` — o transcript
    /// mostra o original, o servidor recebe o expandido). `None` = fio igual
    /// ao texto exibido. O merge do poll compara o eco do servidor contra
    /// ESTE campo (`msg_eq`), então o eco não diverge e o original fica.
    pub wire: Option<String>,
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
    /// linhas prontas + total de linhas visuais (5º campo) JÁ medido na MESMA
    /// largura da chave`. Vive na task da UI (sem compartilhamento entre
    /// tasks). A linha de spinner "working…" é por-frame (o tick muda) e fica
    /// FORA — do conteúdo cacheado e do total memoizado (o render soma a
    /// parte por-frame). O total memoizado evita re-medir O(n) a cada draw.
    pub transcript_cache: Option<(u64, u16, u16, Vec<Line<'static>>, u16)>,
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
    /// Largura interna real do transcript medida no último render (write-back
    /// gêmeo do `last_inner_height`) — o salto da busca mede o prefixo do
    /// histórico na MESMA largura do wrap real.
    pub last_inner_width: u16,
    pub working: bool,
    pub status: String,
    pub tokens: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Métricas do último turno (contrato TUI-visual §1).
    pub last_stats: Option<TurnStats>,
    /// Usage completo da sessão (fim de turno, Ctrl+U, /usage) — alimenta os
    /// overlays com reasoning/cache; `set_usage` guarda só in/out.
    pub last_usage: Option<Usage>,
    /// Overlay modal aberto (`/context`, `/usage`, `/todos`); prioridade no
    /// teclado.
    pub overlay: Option<Overlay>,
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
    /// Sessão pronta p/ turnos (startup instantâneo, Fase V4): false até o
    /// 1º `UiUpdate::NewSession` — header INIT, Enter bloqueado com notice,
    /// poller sem fetch (o watch `sid` sai do placeholder "" no mesmo evento).
    pub session_ready: bool,
    /// Boot de sessão EM VOO (B-3 da V4): true entre o disparo do boot
    /// (startup/Ctrl+N com `session_ready=false`) e a resposta chegar pela
    /// fila (`NewSession` ou falha — `Notice`/`RuntimeDead` em
    /// `apply_ui_update`). Gate do single-flight: Ctrl+N/Enter repetidos não
    /// disparam boots concorrentes.
    pub booting: bool,
    /// Checklist do agente (`/todos`): vem do result do create e do fim de
    /// cada turno (projection do send, quando presente).
    pub todos: Vec<TodoItem>,
    /// Histórico de prompts enviados (mais antigo → mais novo) para o recall
    /// ↑/↓. Cap de 100 (`PROMPT_HISTORY_CAP`), dedupe de igual consecutivo.
    pub prompt_history: Vec<String>,
    /// Posição atual na navegação do histórico; `None` = editando livre.
    pub prompt_history_idx: Option<usize>,
    /// O que estava digitado quando o usuário subiu para o histórico;
    /// restaurado ao descer de volta além do mais novo.
    pub draft: Option<String>,
    /// Busca no transcript (Ctrl+F): linha "find:" aberta ou busca sticky
    /// pós-Enter (ver `SearchState`). `None` = sem busca.
    pub search: Option<SearchState>,
    /// Picker de sessões (`/resume` sem argumento, Fase V5-2): modal
    /// interativo com a lista de `session/list` (↑/↓, Enter, Esc — ver
    /// `SessionPicker`). `None` = fechado. Fica FORA do enum `Overlay`
    /// porque, diferente dos painéis consultivos (qualquer tecla fecha),
    /// aqui as teclas NAVEGAM.
    pub picker: Option<SessionPicker>,
    /// Fila de mensagens (Fase V5-1, estilo Codex/Claude): Enter durante
    /// `working` enfileira o texto (em vez de devolvê-lo); no fim de cada
    /// turno o 1º item é enviado automaticamente (FIFO, um item por turno).
    /// O texto guardado é o ORIGINAL (com `@`), exatamente como digitado.
    pub queue: VecDeque<String>,
    /// Contador de medições O(n) do conteúdo do cache (SÓ testes): sobe apenas
    /// em cache miss — prova que draws consecutivos sem mudança usam o total
    /// memoizado no 5º campo do cache em vez de re-medir o histórico inteiro.
    #[cfg(test)]
    pub transcript_measure_calls: u32,
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
            last_inner_width: 0,
            working: false,
            status: "pronto".to_string(),
            tokens: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            last_stats: None,
            last_usage: None,
            overlay: None,
            ctx: ContextUsage::default(),
            tick: 0,
            pending_compact: false,
            perm_stub: None,
            ctrlc_once: false,
            quit: false,
            read_only,
            runtime_dead: false,
            session_ready: false,
            booting: false,
            todos: Vec::new(),
            prompt_history: Vec::new(),
            prompt_history_idx: None,
            draft: None,
            search: None,
            picker: None,
            queue: VecDeque::new(),
            #[cfg(test)]
            transcript_measure_calls: 0,
        }
    }

    pub fn push_msg(&mut self, role: &str, text: &str) {
        self.push_msg_with_wire(role, text, None);
    }

    /// Variante com texto de FIO (A-1): registra a mensagem com o que foi
    /// EFETIVAMENTE enviado ao servidor (`wire`) SEM mudar o que é exibido —
    /// usado pelo envio com `@arquivo` (transcript guarda o original; o
    /// servidor recebe o expandido e o merge compara o eco contra o fio).
    pub fn push_msg_with_wire(&mut self, role: &str, text: &str, wire: Option<String>) {
        // Sem reset de scroll: com `follow` o render gruda no fim (offset =
        // total − altura da viewport); sem `follow`, o offset absoluto é
        // preservado (drift aceitável ao chegar mensagem nova).
        self.messages.push(ChatMsg {
            role: role.into(),
            text: text.into(),
            kind: MsgKind::Text,
            wire,
        });
        self.bump_rev();
    }

    /// Bump manual da versão do histórico (clear/replace direto de
    /// `messages` — ex. `merge_messages`, Ctrl+N). Invalida o cache.
    pub fn bump_rev(&mut self) {
        self.messages_rev = self.messages_rev.wrapping_add(1);
    }

    /// Sessão pronta (chegou o 1º `UiUpdate::NewSession`): destrava Enter,
    /// tira o header do INIT e libera o fetch do poller (via watch sid).
    /// Idempotente — Ctrl+N em sessão pronta não mexe no estado.
    pub fn mark_session_ready(&mut self) {
        self.session_ready = true;
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

    // ----- fila de mensagens (Enter durante `working`, Fase V5-1) -----

    /// Enfileira um prompt digitado durante o turno (já trimado pelo
    /// chamador; múltiplas linhas permitidas). O consumo é FIFO: um item
    /// por turno, no fim do turno corrente (`commands` faz o spawn).
    pub fn queue_push(&mut self, text: &str) {
        self.queue.push_back(text.to_string());
    }

    /// Gate PURO do consumo de fila: remove e devolve o PRÓXIMO item
    /// (pop-front). O chamador inicia o turno com o texto devolvido; fila
    /// vazia → `None` (nada acontece).
    pub fn next_queued(&mut self) -> Option<String> {
        self.queue.pop_front()
    }

    /// Limpeza deliberada (`/queue clear`): esvazia a fila e devolve quantos
    /// itens foram descartados (p/ a notice).
    pub fn queue_clear(&mut self) -> usize {
        self.queue.drain(..).count()
    }

    // ----- busca no transcript (Ctrl+F): estado + edição da query -----

    /// Abre a linha de busca guardando o buffer do input (esvazia o input —
    /// a linha "find:" toma o lugar dele). Reabre uma busca sticky
    /// preservando a query (cursor no fim).
    pub fn open_search(&mut self) {
        let saved_input = std::mem::take(&mut self.input);
        let saved_cursor = self.cursor;
        self.cursor = 0;
        match &mut self.search {
            Some(s) => {
                s.open = true;
                s.saved_input = saved_input;
                s.saved_cursor = saved_cursor;
                s.qcursor = s.query.chars().count();
            }
            None => {
                self.search = Some(SearchState {
                    query: String::new(),
                    matches: Vec::new(),
                    cursor: 0,
                    qcursor: 0,
                    open: true,
                    saved_input,
                    saved_cursor,
                });
            }
        }
    }

    /// Fecha a linha SEM descartar a busca (vira sticky): restaura o buffer
    /// e o cursor do input. Usado pelo Enter (confirmar) — F3/Shift+F3
    /// seguem navegando e Ctrl+F reabre com a query.
    pub fn close_search_keep(&mut self) {
        if let Some(s) = self.search.as_mut() {
            s.open = false;
            let (buf, cur) = (std::mem::take(&mut s.saved_input), s.saved_cursor);
            self.input = buf;
            self.cursor = cur.min(self.input_len());
        }
    }

    /// Cancela (Esc): fecha, restaura o buffer do input e DESCARTA a busca
    /// inteira (query inclusa — próxima busca começa vazia).
    pub fn cancel_search(&mut self) {
        if let Some(mut s) = self.search.take() {
            self.input = std::mem::take(&mut s.saved_input);
            self.cursor = s.saved_cursor.min(self.input_len());
        }
    }

    /// Enter na busca: confirma o match atual — fecha a linha (sticky),
    /// restaura o buffer e SALTA para a mensagem do match.
    pub fn search_confirm_jump(&mut self) {
        let tem_match = self.search.as_ref().is_some_and(|s| !s.matches.is_empty());
        self.close_search_keep();
        if tem_match {
            let idx = self.search.as_ref().map_or(0, |s| s.cursor);
            self.search_jump_to(idx);
        }
    }

    /// Edição da query: aplica a mutação e recalcula os matches (linear scan
    /// barato) com o cursor de volta ao 1º resultado.
    fn search_edit<F: FnOnce(&mut SearchState)>(&mut self, f: F) {
        if let Some(s) = self.search.as_mut() {
            f(s);
            s.matches = find_matches(&self.messages, &s.query);
            s.cursor = 0;
        }
    }

    pub fn search_insert_char(&mut self, c: char) {
        self.search_edit(|s| {
            let b = qcursor_byte(s);
            s.query.insert(b, c);
            s.qcursor += 1;
        });
    }

    /// Paste na busca: quebras viram espaço (query é single-line por design).
    pub fn search_insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let limpo = text.replace("\r\n", " ").replace(['\r', '\n'], " ");
        self.search_edit(|s| {
            let b = qcursor_byte(s);
            s.query.insert_str(b, &limpo);
            s.qcursor += limpo.chars().count();
        });
    }

    pub fn search_backspace(&mut self) {
        self.search_edit(|s| {
            if s.qcursor == 0 {
                return;
            }
            let b = qcursor_byte(s);
            let prev = s.query[..b].chars().next_back().unwrap();
            s.query.drain(b - prev.len_utf8()..b);
            s.qcursor -= 1;
        });
    }

    pub fn search_delete(&mut self) {
        self.search_edit(|s| {
            let b = qcursor_byte(s);
            if b >= s.query.len() {
                return;
            }
            let next = s.query[b..].chars().next().unwrap();
            s.query.drain(b..b + next.len_utf8());
        });
    }

    pub fn search_move_left(&mut self) {
        if let Some(s) = self.search.as_mut() {
            s.qcursor = s.qcursor.saturating_sub(1);
        }
    }

    pub fn search_move_right(&mut self) {
        if let Some(s) = self.search.as_mut() {
            s.qcursor = (s.qcursor + 1).min(s.query.chars().count());
        }
    }

    pub fn search_move_home(&mut self) {
        if let Some(s) = self.search.as_mut() {
            s.qcursor = 0;
        }
    }

    pub fn search_move_end(&mut self) {
        if let Some(s) = self.search.as_mut() {
            s.qcursor = s.query.chars().count();
        }
    }

    /// Um passo na lista de matches (delta < 0 = anterior, > 0 = próximo) e
    /// SALTA junto (linha aberta: ↑/↓/F3/Shift+F3; sticky: F3/Shift+F3).
    /// Sem wrap — nos extremos fica parado (menos surpresa).
    pub fn search_step(&mut self, delta: i32) {
        let Some(s) = self.search.as_ref() else { return };
        if s.matches.is_empty() {
            return;
        }
        let n = s.matches.len() as i32;
        let novo = (s.cursor as i32 + delta).clamp(0, n - 1) as usize;
        self.search_jump_to(novo);
    }

    /// Salta para o match `idx`: desliga o follow e posiciona o offset no
    /// prefixo do transcript (mensagens [0..msg_idx]) menos meia tela, para
    /// centrar a mensagem do match. O custo O(n) da medição é só aqui,
    /// nunca no caminho do draw.
    pub fn search_jump_to(&mut self, idx: usize) {
        let Some(msg_idx) = self
            .search
            .as_ref()
            .and_then(|s| s.matches.get(idx))
            .map(|m| m.msg_idx)
        else {
            return;
        };
        // Estilos NÃO mudam larguras de conteúdo (badges/separadores são
        // constantes; cores só pintam): qualquer paleta mede o MESMO prefixo.
        // Evita furar `KeyCtx` com a paleta só por causa do salto.
        let pal = crate::ui::theme::Theme::Dark.palette();
        let prefix = transcript_prefix_lines(self, &pal, self.last_inner_width, msg_idx);
        self.follow = false;
        self.scroll = jump_offset(prefix, self.last_inner_height);
        if let Some(s) = self.search.as_mut() {
            s.cursor = idx;
        }
    }

    // ----- picker de sessões (/resume sem argumento, Fase V5-2) -----

    /// Abre o picker em modo Loading (o fetch de `session/list` é spawnado
    /// pelo chamador — mesmo caminho dos outros RPCs). Status honesto
    /// enquanto carrega; nada de RPC no caminho da tecla.
    pub fn open_session_picker(&mut self) {
        self.picker = Some(SessionPicker::loading());
        self.status = "buscando sessões…".to_string();
    }

    /// Fecha o picker SEM escolher (Esc) e devolve o status ao repouso.
    pub fn close_session_picker(&mut self) {
        self.picker = None;
        self.status = "pronto".to_string();
    }

    /// Aplica o resultado do fetch no picker aberto (se ainda estiver — o
    /// usuário pode ter fechado enquanto a busca rodava; aí o resultado é
    /// descartado silenciosamente).
    pub fn fill_session_picker(&mut self, rows: Vec<SessionRow>) {
        if let Some(p) = self.picker.as_mut() {
            p.fill(rows);
            self.status = "sessões — ↑/↓ navega · Enter retoma · Esc fecha".to_string();
        }
    }

    /// Id da sessão selecionada + fecha o picker (Enter). Loading/Empty →
    /// `None` (nada acontece — não há o que retomar).
    pub fn take_selected_session(&mut self) -> Option<String> {
        let id = self.picker.as_ref().and_then(|p| p.selected_id())?;
        self.picker = None;
        Some(id)
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
    "Enter envia · Ctrl+J linha · /compact y/n · Ctrl+F busca · Esc cancela · Ctrl+C 2x sai"
}

pub fn help_bar_for_width(width: usize) -> String {
    let choices = [
        help_bar(),
        "Enter | Ctrl+J | /compact | Ctrl+F busca | Esc | Ctrl+C",
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

// ---------- fila de mensagens: formatters puros (Fase V5-1) ----------

/// Indicador sutil do prompt/spinner quando há itens aguardando:
/// " [fila: N]" (com espaço inicial); fila vazia → string vazia.
pub fn queue_badge(n: usize) -> String {
    if n == 0 {
        String::new()
    } else {
        format!(" [fila: {n}]")
    }
}

// ---------- overlays (/context, /usage): formatters puros ----------

/// Contagem de tokens em formato curto: 999 → "999", 240_500 → "240.5K",
/// 1_200_000 → "1.2M" (K/M apenas — é o que o protocolo expõe na prática).
pub fn fmt_tokens(u: u64) -> String {
    if u < 1_000 {
        u.to_string()
    } else if u < 1_000_000 {
        format!("{:.1}K", u as f64 / 1_000.0)
    } else {
        format!("{:.1}M", u as f64 / 1_000_000.0)
    }
}

/// Cache hit aproximado (0–100): `cache_read / (cache_read + input) ×100` —
/// leituras de cache sobre tudo que precisou ir à rede. Sem dados → 0.0.
pub fn cache_hit(u: &Usage) -> f64 {
    let denom = u.cache_read_tokens.saturating_add(u.input_tokens);
    if denom == 0 {
        0.0
    } else {
        u.cache_read_tokens as f64 / denom as f64 * 100.0
    }
}

/// Resumo textual de contexto p/ o REPL (`/context` imprime isto; sem RPC —
/// a projection vem do create) e p/ o título à direita do overlay da TUI.
pub fn context_summary_line(ctx: &ContextUsage) -> String {
    if ctx.window == 0 {
        "contexto: sem dados (projection ausente)".to_string()
    } else {
        format!(
            "contexto: {}/{} ({:.1}%)",
            fmt_tokens(ctx.used),
            fmt_tokens(ctx.window),
            ctx.pct()
        )
    }
}

/// Contrato do gate de overlay (a política real vive em
/// commands::handle_key_event): com overlay aberto, qualquer evento o FECHA;
/// tudo é consumido EXCETO Ctrl+C, que fecha o modal e segue para o fluxo de
/// saída. Helper de teste do campo; mantido #[cfg(test)].
#[cfg(test)]
pub(crate) fn consume_key_for_overlay(app: &mut TuiApp) -> bool {
    if app.overlay.is_some() {
        app.overlay = None;
        true
    } else {
        false
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
/// logo fica fora do cache do transcript. Com fila pendente, o badge
/// " [fila: N]" acompanha (também por frame — nada cacheado).
fn working_line(app: &TuiApp, pal: &Palette) -> Line<'static> {
    Line::from(Span::styled(
        format!(
            "{} working… (Esc cancela){}",
            spinner_frame(app.tick),
            queue_badge(app.queue.len())
        ),
        Style::default().fg(pal.system).add_modifier(Modifier::BOLD),
    ))
}

/// Transcript da viewport COM CACHE por `(messages_rev, largura, altura)`:
/// sem mudança de histórico ou de geometria, nenhum re-parse de markdown nem
/// re-amostragem da splash (o mesmo contrato de cache de `ui::art`) — e o
/// TOTAL de linhas visuais vem memoizado no cache: a medição O(n) de
/// `transcript_total_lines` só roda em cache miss (antes ela rodava a CADA
/// draw — com spinner a 10fps eram 10 medições/s do histórico inteiro).
/// Devolve `(texto pronto, total)`: o total já inclui as linhas por-frame
/// (spinner), medidas em O(1) por fora do cache — cada linha quebra de forma
/// independente no wrap do Paragraph, então o total do texto completo é a
/// SOMA das partes (propriedade garantida por teste de consistência).
fn transcript_text(
    app: &mut TuiApp,
    pal: &Palette,
    transcript_width: usize,
    transcript_height: u16,
) -> (Text<'static>, u16) {
    let key = (app.messages_rev, transcript_width as u16, transcript_height);
    // Hit na chave → linhas E total memoizados juntos (mesma largura no wrap
    // do conteúdo e na medição, por construção: ambos saem do mesmo build).
    let (mut lines, mut total) = if let Some(c) = app
        .transcript_cache
        .as_ref()
        .filter(|c| (c.0, c.1, c.2) == key)
    {
        (c.3.clone(), c.4)
    } else {
        let built = history_lines(app, pal, transcript_width, transcript_height, false);
        let measured = transcript_total_lines(&Text::from(built.clone()), key.1);
        app.transcript_cache = Some((key.0, key.1, key.2, built.clone(), measured));
        #[cfg(test)]
        {
            app.transcript_measure_calls += 1;
        }
        (built, measured)
    };
    // Extras POR-FRAME (nunca cacheados: mudam a cada tick).
    let mut extras: Vec<Line<'static>> = Vec::new();
    if lines.is_empty() {
        // O footer concentra os atalhos; o histórico vazio não os duplica.
        extras.push(Line::from(Span::styled(
            "Nenhuma mensagem ainda.",
            Style::default().fg(pal.system).add_modifier(Modifier::BOLD),
        )));
    }
    if app.working {
        extras.push(working_line(app, pal));
    }
    if !extras.is_empty() {
        // Medir SÓ os extras é O(1); o total memoizado do grosso segue intacto.
        total = total.saturating_add(transcript_total_lines(&Text::from(extras.clone()), key.1));
        lines.extend(extras);
    }
    (Text::from(lines), total)
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
        // init = 1ª sessão nascendo (startup instantâneo), antes de working.
        let state = if self.runtime_dead {
            "DEAD"
        } else if !self.session_ready {
            "init"
        } else if self.working {
            "working"
        } else {
            "pronto"
        };
        format!(
            "{state} | ctx={:.0}% | model={} | {}{}",
            self.ctx.pct(),
            self.model,
            self.status,
            queue_badge(self.queue.len())
        )
    }
}

fn input_title(app: &TuiApp, compact: bool, width: usize) -> String {
    // Fila pendente aparece como badge sutil no título do prompt (working
    // ou não — consumo só acontece no fim de turno; fora dele é lembrete).
    let badge = queue_badge(app.queue.len());
    let title = if app.search.as_ref().is_some_and(|s| s.open) {
        "find: Enter salta · Esc cancela · ↑/↓ e F3 trocam".to_string()
    } else if app.pending_compact {
        "compact: y confirma / n cancela".to_string()
    } else if app.perm_stub.is_some() && compact {
        "permissao: y confirma / n cancela".to_string()
    } else if app.working {
        format!("input: aguarde; Esc cancela{badge}")
    } else if !badge.is_empty() {
        format!("input: Enter envia; Ctrl+J quebra linha{badge}")
    } else {
        "input: Enter envia; Ctrl+J quebra linha".to_string()
    };
    truncate_cells(&title, width.saturating_sub(2))
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
    // INIT (1ª sessão nascendo — startup instantâneo) vem antes de working/ready.
    let state = if app.runtime_dead {
        "DEAD"
    } else if !app.session_ready {
        "INIT"
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
            " zcode-cli tui | session={} | mode={} | model={} | state={} ",
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

/// Nº de linhas visuais do PREFIXO [0..msg_idx) do transcript na MESMA
/// largura interna do wrap real (write-back `last_inner_width`). Construção
/// idêntica ao render (`history_lines` sem splash + `transcript_total_lines`
/// na mesma largura). Mutação temporária: corta o rabo, mede, recola — a
/// versão do histórico não muda (cache intacto). Custo O(n) só no salto da
/// busca (Ctrl+F/Enter/F3), nunca no caminho do draw.
pub fn transcript_prefix_lines(
    app: &mut TuiApp,
    pal: &Palette,
    inner_width: u16,
    msg_idx: usize,
) -> u16 {
    if inner_width == 0 {
        return 0;
    }
    let corte = msg_idx.min(app.messages.len());
    // Prefixo sem conteúdo renderizável (vazio ou só splash): 0 — o
    // transcript real não renderiza o placeholder quando há histórico.
    if !app.messages[..corte].iter().any(|m| !is_splash(&m.text)) {
        return 0;
    }
    let mut rabo = app.messages.split_off(corte);
    let lines = history_lines(app, pal, inner_width as usize, u16::MAX, false);
    let prefix = transcript_total_lines(&Text::from(lines), inner_width);
    app.messages.append(&mut rabo);
    prefix
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
    // Total memoizado no cache do transcript: sem isso a medição O(n)
    // (`Paragraph::line_count` sobre o histórico inteiro) rodava a CADA draw.
    let (text, total) = transcript_text(app, pal, inner_width as usize, inner_height);
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
    // Largura interna real junto (gêmeo do de cima): o salto da busca mede o
    // prefixo do histórico exatamente na largura do wrap deste frame.
    app.last_inner_width = inner_width;
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

/// Prefixo da linha de busca (accent) — largura fixa p/ o cursor.
const FIND_PREFIX: &str = "find: ";

/// Linha de busca inline (estilo less/vim): prefixo "find: " em accent +
/// query + status do match, no lugar do input enquanto a busca está aberta.
fn search_line(app: &TuiApp, pal: &Palette) -> Line<'static> {
    let Some(s) = app.search.as_ref() else {
        return Line::from("");
    };
    Line::from(vec![
        Span::styled(
            FIND_PREFIX,
            Style::default().fg(pal.user).add_modifier(Modifier::BOLD),
        ),
        Span::raw(s.query.clone()),
        Span::styled(
            format!(" {}", search_status(s)),
            Style::default().fg(pal.muted),
        ),
    ])
}

/// Célula visual do cursor da busca: prefixo "find: " + query até o cursor
/// (linha única — y é sempre 0).
fn search_cursor_cell(app: &TuiApp) -> usize {
    app.search
        .as_ref()
        .map(|s| {
            let ate: String = s.query.chars().take(s.qcursor).collect();
            cell_width(FIND_PREFIX) + cell_width(&ate)
        })
        .unwrap_or(0)
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
    // Busca aberta: a linha find: substitui o texto do input (o buffer está
    // guardado no SearchState e volta ao fechar) — cursor na query.
    let searching = app.search.as_ref().is_some_and(|s| s.open);
    let (line, cursor_cell) = if searching {
        (0usize, search_cursor_cell(app))
    } else {
        (app.line_col().0, input_visual_cursor_cell(app))
    };
    let horizontal_scroll = cursor_cell.saturating_sub(inner_width.saturating_sub(1));
    let vertical_scroll = line.saturating_sub(inner_height.saturating_sub(1));
    let title = input_title(app, compact, area.width as usize);
    let corpo = if searching {
        Text::from(search_line(app, pal))
    } else {
        input_text(app, pal)
    };
    let mut input = Paragraph::new(corpo)
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
    // Overlay aberto cobre o prompt: cursor de hardware escondido (evita o
    // ponteiro piscando sobre a janela modal).
    // Overlay/picker aberto cobre o prompt: cursor de hardware escondido
    // (evita o ponteiro piscando sobre a janela modal).
    if app.overlay.is_none() && app.picker.is_none() {
        f.set_cursor_position((origin_x + x as u16, origin_y + y as u16));
    }
}

fn render_minimal(f: &mut Frame, app: &TuiApp, pal: &Palette) {
    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let searching = app.search.as_ref().is_some_and(|s| s.open);
    let primary = if searching {
        // Tela mínima: a linha de busca também substitui o input (a query +
        // status na única linha disponível).
        let s = app.search.as_ref().unwrap();
        format!("{}{} {}", FIND_PREFIX, s.query, search_status(s))
    } else if app.pending_compact {
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
    if app.overlay.is_some() || app.picker.is_some() {
        return; // overlay/picker cobre o prompt: cursor de hardware escondido
    }
    let x = if searching {
        search_cursor_cell(app)
    } else {
        input_visual_cursor_cell(app)
    }
    .min(area.width.saturating_sub(1) as usize) as u16;
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
    // Overlays (/context, /usage, /todos) POR CIMA de qualquer modo, desenhados
    // por último com Clear (a área total é a referência do centro; o transcript
    // continua renderizado embaixo, preservando o write-back de scroll).
    match app.overlay {
        Some(Overlay::Context) => render_context_overlay(app, f, area, pal),
        Some(Overlay::Usage) => render_usage_overlay(app, f, area, pal),
        Some(Overlay::Todos) => render_todos_overlay(app, f, area, pal),
        None => {}
    }
    // Picker de sessões (/resume, Fase V5-2): por cima de TUDO (inclusive dos
    // overlays — é modal de verdade), mesma área de referência.
    if let Some(p) = app.picker.as_ref() {
        render_sessions_picker(p, f, area, pal);
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

// ---------- overlays (/context, /usage): render ----------

/// Área centrada de ~60% da largura e altura fixa (clampada à área de
/// referência; largura mínima 20 p/ o conteúdo fazer sentido).
fn overlay_area(area: Rect, height: u16) -> Rect {
    let max_w = area.width as usize;
    let w = ((max_w * 60) / 100)
        .max(20.min(max_w))
        .min(max_w) as u16;
    let h = height.min(area.height);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}

/// Barra de progresso de 1 linha exatamente na largura dada: `█` preenchido
/// + `░` restante em spans coloridos (chars manuais em vez de Gauge — controle
/// total de cor pela paleta; cor de preenchimento = `pal.user`, ciano/azul no
/// Dark e verde no Retro, restante esmaecido em `pal.muted`).
fn progress_line(pct: f64, width: usize, pal: &Palette) -> Line<'static> {
    let width = width.max(1);
    let ratio = (pct.clamp(0.0, 100.0) / 100.0).min(1.0);
    let filled = ((ratio * width as f64).round() as usize).min(width);
    Line::from(vec![
        Span::styled("█".repeat(filled), Style::default().fg(pal.user)),
        Span::styled("░".repeat(width - filled), Style::default().fg(pal.muted)),
    ])
}

/// Linha da legenda do /context: ponto colorido + nome + % sobre o total da
/// sessão + valor absoluto. Sem usage (`total == 0`) → "—" honesto.
fn legend_line(name: &str, dot: Color, tokens: u64, total: u64, pal: &Palette) -> Line<'static> {
    let (pct, abs) = if total > 0 {
        (
            format!("{:.1}%", tokens as f64 / total as f64 * 100.0),
            fmt_tokens(tokens),
        )
    } else {
        ("—".to_string(), "—".to_string())
    };
    Line::from(vec![
        Span::styled("● ", Style::default().fg(dot)),
        Span::styled(format!("{name:<15}"), Style::default().fg(pal.assistant)),
        Span::styled(format!("{pct:>6}"), Style::default().fg(pal.assistant)),
        Span::raw(format!(" ({abs})")),
    ])
}

/// Overlay `/context` (referência visual: painel "Context windows" do Claude
/// Code): janela centrada com Clear, título "Context" à esquerda e o resumo
/// "used/window (pct)" à direita, barra de progresso da largura interna,
/// legenda por tipo de token (% sobre total_tokens) e rodapé com cache hit
/// aproximado. Sem projection → mensagem honesta em vez de barra zerada.
/// PURA: não muta `app` (não toca no write-back de scroll do transcript).
pub fn render_context_overlay(app: &TuiApp, f: &mut Frame, area: Rect, pal: &Palette) {
    if area.width < 20 || area.height < 5 {
        return; // tela mínima: nem o bloco cabe — overlay fica invisível
    }
    let overlay = overlay_area(area, 11);
    f.render_widget(Clear, overlay);
    let has_ctx = app.ctx.window > 0;
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(pal.border))
        .title(Line::from(" Context ").left_aligned())
        .title_style(Style::default().fg(pal.user).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(pal.surface))
        .padding(Padding::horizontal(1));
    if has_ctx {
        block = block.title(
            Line::from(format!(
                " {}/{} ({:.1}%) ",
                fmt_tokens(app.ctx.used),
                fmt_tokens(app.ctx.window),
                app.ctx.pct()
            ))
            .right_aligned(),
        );
    }
    let inner = block.inner(overlay);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if has_ctx {
        lines.push(progress_line(app.ctx.pct(), inner.width as usize, pal));
    } else {
        lines.push(Line::from(Span::styled(
            "sem dados de contexto (projection ausente)",
            Style::default().fg(pal.system).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(""));
    // Legenda: o que o protocolo dá em session/usage, % sobre total_tokens.
    let u = app.last_usage.as_ref();
    let total = u.map_or(0, |x| x.total_tokens);
    let entries: [(&str, u64, Color); 5] = [
        ("input", u.map_or(0, |x| x.input_tokens), pal.user),
        ("output", u.map_or(0, |x| x.output_tokens), pal.assistant),
        ("reasoning", u.map_or(0, |x| x.reasoning_tokens), pal.system),
        ("cache read", u.map_or(0, |x| x.cache_read_tokens), pal.badge_fg),
        (
            "cache creation",
            u.map_or(0, |x| x.cache_creation_tokens),
            pal.prompt,
        ),
    ];
    for (name, tokens, dot) in entries {
        lines.push(legend_line(name, dot, tokens, total, pal));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("cache hit (aprox.): {:.1}%", u.map_or(0.0, cache_hit)),
        Style::default().fg(pal.muted),
    )));
    f.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .style(Style::default().fg(pal.assistant).bg(pal.surface)),
        overlay,
    );
}

/// Cartão do overlay `/usage` (Block rounded com título curto).
fn card_block(title: &str, pal: &Palette) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(pal.border))
        .title(format!(" {title} "))
        .title_style(Style::default().fg(pal.user).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(pal.surface))
}

/// Overlay `/usage` (referência visual: dois cartões lado a lado do /usage do
/// Claude Code): "Session" (total/in/out/reqs/modelo) e "Last turn"
/// (in/out/tok-s/elapsed), com rodapé honesto — a cota do plano (5h/semanal)
/// NÃO é exposta pelo protocolo. Sem dados → placeholders "—" claros.
/// PURA: não muta `app`.
pub fn render_usage_overlay(app: &TuiApp, f: &mut Frame, area: Rect, pal: &Palette) {
    if area.width < 20 || area.height < 5 {
        return;
    }
    let overlay = overlay_area(area, 10);
    f.render_widget(Clear, overlay);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(pal.border))
        .title(Line::from(" Usage ").left_aligned())
        .title_style(Style::default().fg(pal.user).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(pal.surface))
        .padding(Padding::horizontal(1));
    // Bloco externo primeiro (bordas + título + fundo); cartões por cima da
    // área interna dele.
    f.render_widget(block.clone(), overlay);
    let inner = block.inner(overlay);
    // Cartões (7 linhas = 5 de conteúdo + 2 de borda) + rodapé de 1 linha.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(7), Constraint::Length(1)])
        .split(inner);
    let cards = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);

    let u = app.last_usage.as_ref();
    let traço = || "—".to_string();
    let session_lines: Vec<Line<'static>> = vec![
        format!(
            "total  {}",
            u.map(|x| fmt_tokens(x.total_tokens)).unwrap_or_else(traço)
        ),
        format!(
            "in     {}",
            u.map(|x| fmt_tokens(x.input_tokens)).unwrap_or_else(traço)
        ),
        format!(
            "out    {}",
            u.map(|x| fmt_tokens(x.output_tokens)).unwrap_or_else(traço)
        ),
        format!(
            "reqs   {}",
            u.map(|x| x.model_request_count.to_string())
                .unwrap_or_else(traço)
        ),
        format!(
            "model  {}",
            if app.model.is_empty() {
                "—".to_string()
            } else {
                app.model.clone()
            }
        ),
    ]
    .into_iter()
    .map(Line::from)
    .collect();
    let st = app.last_stats.as_ref();
    let turn_lines: Vec<Line<'static>> = vec![
        format!(
            "in     {}",
            st.map(|s| fmt_tokens(s.input_tokens)).unwrap_or_else(traço)
        ),
        format!(
            "out    {}",
            st.map(|s| fmt_tokens(s.output_tokens)).unwrap_or_else(traço)
        ),
        format!(
            "veloc  {}",
            st.map(|s| {
                if s.tok_per_s.is_finite() {
                    format!("{:.1} tok/s", s.tok_per_s)
                } else {
                    "—".to_string()
                }
            })
            .unwrap_or_else(traço)
        ),
        format!(
            "tempo  {}",
            st.map(|s| format!("{:.1}s", s.elapsed_ms as f64 / 1000.0))
                .unwrap_or_else(traço)
        ),
        String::new(),
    ]
    .into_iter()
    .map(Line::from)
    .collect();

    let label_style = Style::default().fg(pal.assistant).bg(pal.surface);
    for (area_card, title, lines) in [
        (cards[0], "Session", session_lines),
        (cards[1], "Last turn", turn_lines),
    ] {
        let card = card_block(title, pal);
        f.render_widget(
            Paragraph::new(Text::from(lines))
                .block(card)
                .style(label_style),
            area_card,
        );
    }
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "cota do plano (5h/semanal): não exposta pelo protocolo",
            Style::default().fg(pal.muted),
        )))
        .style(Style::default().fg(pal.muted).bg(pal.surface)),
        rows[1],
    );
}

/// Overlay `/todos` (checklist do agente): janela centrada com Clear, título
/// "Todos" à esquerda + contagem "feitos/total" à direita, um item por linha
/// com checkbox `[ ]`/`[~]`/`[x]` (Pending/InProgress/Completed). Em andamento
/// em destaque (accent + BOLD, checkbox `[~]` na cor accent); concluído
/// esmaecido; vazio → mensagem honesta. Lista maior que a janela é cortada
/// com contador "… +N mais" (sem scroll — painel consultivo).
/// PURA: não muta `app` (não toca no write-back de scroll do transcript).
pub fn render_todos_overlay(app: &TuiApp, f: &mut Frame, area: Rect, pal: &Palette) {
    if area.width < 20 || area.height < 5 {
        return; // tela mínima: nem o bloco cabe — overlay fica invisível
    }
    let total = app.todos.len();
    let feitos = app
        .todos
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count();
    // Altura: 1 linha por item + moldura (2) + respiro (2); mínimo p/ vazio.
    let altura = (total as u16).saturating_add(4).max(6);
    let overlay = overlay_area(area, altura);
    f.render_widget(Clear, overlay);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(pal.border))
        .title(Line::from(" Todos ").left_aligned())
        .title_style(Style::default().fg(pal.user).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(pal.surface))
        .padding(Padding::horizontal(1));
    if total > 0 {
        block = block.title(Line::from(format!(" {feitos}/{total} ")).right_aligned());
    }
    let inner = block.inner(overlay);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if total == 0 {
        lines.push(Line::from(Span::styled(
            "sem todos registrados (o agente ainda não planejou)",
            Style::default().fg(pal.muted),
        )));
    } else {
        // "[] " + folga p/ o conteúdo não encostar na borda.
        let content_width = inner.width.saturating_sub(5) as usize;
        let max_visiveis = inner.height as usize;
        for (i, t) in app.todos.iter().enumerate() {
            // Última linha visível vira o corte quando a lista não cabe.
            if i + 1 >= max_visiveis && total > max_visiveis {
                lines.push(Line::from(Span::styled(
                    format!("… +{} mais", total - i),
                    Style::default().fg(pal.muted),
                )));
                break;
            }
            let (marca, marca_fg) = match t.status {
                TodoStatus::Pending => ("[ ]", pal.assistant),
                // Em andamento: checkbox `[~]` na cor accent (pal.user).
                TodoStatus::InProgress => ("[~]", pal.user),
                TodoStatus::Completed => ("[x]", pal.muted),
            };
            // Item em andamento em destaque; concluído esmaecido.
            let estilo = match t.status {
                TodoStatus::InProgress => {
                    Style::default().fg(pal.user).add_modifier(Modifier::BOLD)
                }
                TodoStatus::Completed => Style::default().fg(pal.muted),
                TodoStatus::Pending => Style::default().fg(pal.assistant),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{marca} "), Style::default().fg(marca_fg)),
                Span::styled(truncate_cells(&t.content, content_width), estilo),
            ]));
        }
    }
    f.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .style(Style::default().fg(pal.assistant).bg(pal.surface)),
        overlay,
    );
}

// ---------- picker de sessões (/resume): render ----------

/// Passo de página do picker (PageUp/PageDown): fixo em 10 — a altura real da
/// viewport do overlay não chega no handler de teclas (e listas de sessão
/// raramente passam disso).
pub const PICKER_PAGE_STEP: i32 = 10;

/// Janela visível da lista do picker (puro, testável): devolve o recorte
/// `[start, end)` que SEMPRE contém `selected` quando couber. Stateless
/// (derivado do selected a cada frame — sem scroll state extra): seleção
/// abaixo da viewport vira âncora na ÚLTIMA linha visível; acima, na
/// PRIMEIRA. Listas que cabem inteiras → recorte total.
fn picker_window(total: usize, selected: usize, vis: usize) -> (usize, usize) {
    if total == 0 || vis == 0 {
        return (0, 0);
    }
    if total <= vis {
        return (0, total);
    }
    let start = selected.saturating_sub(vis.saturating_sub(1));
    (start, (start + vis).min(total))
}

/// Uma linha do picker: `> ` na selecionada + título (flexível) + id curto
/// + status + data — colunas truncadas em células (graphemes), estilo por
/// papel: selecionada em accent (pal.user + BOLD); demais em assistant com
/// metadados muted.
fn picker_row_line(
    row: &SessionRow,
    selected: bool,
    width: usize,
    pal: &Palette,
) -> Line<'static> {
    let prefix = if selected { "> " } else { "  " };
    // Colunas fixas à direita quando couberem; o título leva o resto (mínimo
    // 8 células — em telas estreitas só o título degrada, nunca estoura).
    // Data: corte duro em 10 células (a parte da data do RFC3339) — sufixo
    // "..." aqui desperdiçaria a coluna sem ajudar a leitura.
    let data: String = row.created.chars().take(10).collect();
    let status = truncate_cells(&row.status, 8);
    let id8 = crate::session::sid_short8(&row.id);
    let fixos = cell_width(&data) + 1 + cell_width(&status) + 1 + cell_width(&id8) + 1;
    let title_w = width.saturating_sub(cell_width(prefix) + fixos).max(8);
    let title = truncate_cells(&row.title, title_w);
    let meta_style = if selected {
        Style::default().fg(pal.user)
    } else {
        Style::default().fg(pal.muted)
    };
    let title_style = if selected {
        Style::default().fg(pal.user).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(pal.assistant)
    };
    Line::from(vec![
        Span::styled(prefix.to_string(), Style::default().fg(pal.user)),
        Span::styled(title, title_style),
        Span::styled(format!(" {id8} "), meta_style),
        Span::styled(status, meta_style),
        Span::styled(format!(" {data}"), meta_style),
    ])
}

/// Overlay do picker de sessões (`/resume` sem argumento, Fase V5-2). Visual
/// da família Todos/Context (janela centrada, Clear, Rounded); INTERATIVO no
/// teclado (o gate do picker em `handle_key_event` decide as teclas — por
/// isso NÃO é um `Overlay` do enum, que fecha com qualquer tecla). Loading →
/// "buscando sessões…"; vazio → "nenhuma sessão encontrada"; itens → janela
/// deslizante ao redor da seleção (`picker_window`) + contador "sel/total" no
/// título à direita (o indicador do recorte). PURA: não muta `app`.
pub fn render_sessions_picker(p: &SessionPicker, f: &mut Frame, area: Rect, pal: &Palette) {
    if area.width < 20 || area.height < 5 {
        return; // tela mínima: nem o bloco cabe — picker fica invisível
    }
    let total = p.len();
    // Altura: 1 linha por item + moldura (2) + dica (1); mínimo p/ estados.
    let altura = if total == 0 {
        6
    } else {
        (total as u16 + 3).min(area.height)
    };
    let overlay = overlay_area(area, altura);
    f.render_widget(Clear, overlay);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(pal.border))
        .title(Line::from(" Sessões ").left_aligned())
        .title_style(Style::default().fg(pal.user).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(pal.surface))
        .padding(Padding::horizontal(1));
    if !p.is_empty() {
        block = block
            .title(Line::from(format!(" {}/{} ", p.selected + 1, total)).right_aligned());
    }
    let inner = block.inner(overlay);
    // Linha de dica reservada na base (1 linha); a lista usa o resto (o
    // Paragraph da lista é renderizado com o block na área do overlay — as
    // linhas caem no interior dele, limitadas por `list_h` na janela).
    let list_h = inner.height.saturating_sub(1);
    let hint_h = inner.height - list_h;
    let hint_area = Rect {
        y: inner.y + list_h,
        height: hint_h,
        ..inner
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    match &p.data {
        PickerData::Loading => {
            lines.push(Line::from(Span::styled(
                "buscando sessões…",
                Style::default().fg(pal.muted),
            )));
        }
        PickerData::Empty => {
            lines.push(Line::from(Span::styled(
                "nenhuma sessão encontrada",
                Style::default().fg(pal.muted),
            )));
        }
        PickerData::Items(rows) => {
            let (start, end) = picker_window(rows.len(), p.selected, list_h as usize);
            for (i, row) in rows[start..end].iter().enumerate() {
                lines.push(picker_row_line(
                    row,
                    start + i == p.selected,
                    inner.width as usize,
                    pal,
                ));
            }
        }
    }
    f.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .style(Style::default().fg(pal.assistant).bg(pal.surface)),
        overlay,
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑/↓ navega · PgUp/PgDn página · Enter retoma · Esc fecha",
            Style::default().fg(pal.muted).bg(pal.surface),
        ))),
        hint_area,
    );
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
        // Dentro do fence com linguagem CONHECIDA: highlight syntect (V5-2) —
        // spans 1:1 recompõem a linha, bg = code_bg em TODOS os spans e ao
        // menos um fg explícito da paleta (o `let` é keyword/storage).
        let codigo = &rendered[2];
        assert_eq!(texto(codigo), "let x = 1;");
        assert!(codigo.iter().all(|s| s.style.bg == Some(pal.code_bg)));
        assert!(
            codigo.iter().any(|s| s.style.fg.is_some()),
            "highlight aplica fg da paleta: {:?}",
            codigo
        );
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
    fn markdown_rico_fence_sem_linguagem_ou_desconhecida_fica_crua() {
        use crate::ui::theme::Theme;
        let pal = Theme::Dark.palette();
        let texto =
            |l: &[Span]| l.iter().map(|s| s.content.as_ref()).collect::<String>();
        // Fence NUA (sem linguagem): corpo cru com bg e SEM fg — o caminho
        // de sempre, preservado pelo highlight (V5-2).
        let nua = markdown_message_spans("```\ncru e simples\n```", &pal);
        assert_eq!(nua.len(), 3);
        let corpo = &nua[1];
        assert_eq!(texto(corpo), "cru e simples");
        assert_eq!(corpo[0].style.bg, Some(pal.code_bg));
        assert_eq!(corpo[0].style.fg, None, "código sem linguagem não ganha fg");
        // Linguagem DESCONHECIDA: mesmo caminho cru (degradação honesta).
        let estranha = markdown_message_spans("```zesperanto\nnada reconhecido\n```", &pal);
        assert_eq!(estranha.len(), 3);
        assert_eq!(texto(&estranha[1]), "nada reconhecido");
        assert_eq!(estranha[1][0].style.bg, Some(pal.code_bg));
        assert_eq!(estranha[1][0].style.fg, None);
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
            wire: None,
        });
        let lines = history_lines(&app, &pal, 80, u16::MAX, false);
        assert!(lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains("THINK"))));
        // Corpo do código dentro da fence: bg do código E DIM/ITALIC. Com o
        // highlight (V5-2) a linha pode vir party em vários spans — a linha
        // é encontrada pelo texto RECOMPOSTO e o DIM/ITALIC do reasoning
        // sobrevive em todos os spans (o patch do base preserva o fg próprio
        // do highlight, mas o DIM/ITALIC do papel continua presente).
        let codigo = lines
            .iter()
            .find(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .trim()
                    == "fn f() {}"
            })
            .expect("linha de código presente");
        assert_eq!(
            codigo
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .trim(),
            "fn f() {}"
        );
        // O bg do código está em TODOS os spans do highlight (o 1º span da
        // continuação é o prefixo cru "   ", sem estilo — igual ao caminho
        // antigo, onde ele também nunca teve DIM/bg). Todo span do highlight
        // mantém bg do código + DIM/ITALIC do reasoning (o patch do base
        // preserva o fg próprio do highlight E os modifiers do papel).
        let trechos = codigo
            .spans
            .iter()
            .filter(|s| s.style.bg.is_some())
            .collect::<Vec<_>>();
        assert!(!trechos.is_empty(), "highlight presente na linha");
        assert!(trechos.iter().all(|s| s.style.bg == Some(pal.code_bg)));
        assert!(trechos
            .iter()
            .all(|s| s.style.add_modifier.contains(Modifier::DIM)));
        assert!(trechos
            .iter()
            .all(|s| s.style.add_modifier.contains(Modifier::ITALIC)));
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
        assert!(
            rows.iter().any(|row| row.contains("zcode-cli tui")),
            "header se identifica como TUI (não só como zcode-cli)"
        );
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
        let (t, _) = transcript_text(&mut app, &pal, 50, 8);
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
    fn transcript_total_memoizado_duas_renders_sem_mudanca() {
        use crate::ui::theme::Theme;
        let pal = Theme::Dark.palette();
        let mut app = TuiApp::new("sess_memo", "w", false);
        app.push_msg("assistant", &"linha longa de conteúdo ".repeat(40)); // wrap
        // 1ª render: cache miss → mede o histórico UMA vez (e guarda o total).
        let medicoes0 = app.transcript_measure_calls;
        let (t1, total1) = transcript_text(&mut app, &pal, 60, 10);
        assert_eq!(app.transcript_measure_calls, medicoes0 + 1);
        assert!(total1 > 10, "conteúdo quebra além da viewport");
        // 2ª render SEM mudança (mesma rev/largura/altura): hit no cache →
        // NENHUMA medição nova e o total é o valor memoizado (idêntico).
        let (_t2, total2) = transcript_text(&mut app, &pal, 60, 10);
        assert_eq!(
            app.transcript_measure_calls,
            medicoes0 + 1,
            "draw seguido não re-mede O(n) — usa o total cacheado"
        );
        assert_eq!(total1, total2);
        // Consistência: o total memoizado bate com a medição independente do
        // texto completo na MESMA largura do wrap (o cache guarda os dois).
        assert_eq!(total1, transcript_total_lines(&t1, 60));
        // Mudança de chave (push bumpa rev) → próxima render mede de novo.
        app.push_msg("user", "oi");
        let _ = transcript_text(&mut app, &pal, 60, 10);
        assert_eq!(app.transcript_measure_calls, medicoes0 + 2);
    }

    #[test]
    fn transcript_total_com_extras_por_frame_e_aditividade() {
        use crate::ui::theme::Theme;
        let pal = Theme::Dark.palette();
        let mut app = TuiApp::new("sess_memo2", "w", false);
        app.push_msg("user", &"palavra comprida ".repeat(30));
        app.working = true;
        // Total devolvido = cacheado + spinner por-frame: IGUAL à medição do
        // texto COMPLETO (propriedade de aditividade do wrap por linha).
        let (t, total) = transcript_text(&mut app, &pal, 30, 10);
        assert_eq!(total, transcript_total_lines(&t, 30));
        // O tick anima o spinner sem invalidar o cache (mesma chave) — o
        // total continua consistente com o texto completo do frame.
        let cache_antes = app.transcript_cache.as_ref().unwrap().3.clone();
        app.tick += 1;
        let (t2, total2) = transcript_text(&mut app, &pal, 30, 10);
        assert_eq!(app.transcript_cache.as_ref().unwrap().3, cache_antes);
        assert_eq!(total2, transcript_total_lines(&t2, 30));
        // Largura ESTREITA força o spinner a quebrar em 2+ linhas: a soma
        // continua batendo (extras medidos na mesma largura da chave).
        let (t3, total3) = transcript_text(&mut app, &pal, 12, 10);
        assert!(transcript_total_lines(&t3, 12) > 1);
        assert_eq!(total3, transcript_total_lines(&t3, 12));
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
            wire: None,
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
        // Fixture de sessão PRONTA: o alvo do teste é o toggle DEAD, não o
        // gate INIT do startup instantâneo (coberto em header_init_…).
        let mut app = TuiApp::new("sess_dead", "w", false);
        app.mark_session_ready();
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

    // ----- overlays (/context, /usage): formatters e estado -----

    #[test]
    fn fmt_tokens_fronteiras_k_m() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(42), "42");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_000), "1.0K");
        assert_eq!(fmt_tokens(240_500), "240.5K");
        assert_eq!(fmt_tokens(999_999), "1000.0K");
        assert_eq!(fmt_tokens(1_000_000), "1.0M");
        assert_eq!(fmt_tokens(1_200_000), "1.2M");
        assert_eq!(fmt_tokens(123_456_789), "123.5M");
    }

    #[test]
    fn cache_hit_sem_cache_zero_e_proporcoes() {
        // Sem dados (default) → 0.0 honesto, nunca NaN.
        assert_eq!(cache_hit(&Usage::default()), 0.0);
        // Metade cache → 50; 3/4 cache → 75 (floats exatos em binário).
        let metade = Usage {
            input_tokens: 100,
            cache_read_tokens: 100,
            ..Default::default()
        };
        assert_eq!(cache_hit(&metade), 50.0);
        let tri = Usage {
            input_tokens: 100,
            cache_read_tokens: 300,
            ..Default::default()
        };
        assert_eq!(cache_hit(&tri), 75.0);
        // Só output (sem input nem cache) → 0.0.
        let so_out = Usage {
            output_tokens: 500,
            ..Default::default()
        };
        assert_eq!(cache_hit(&so_out), 0.0);
    }

    #[test]
    fn context_summary_line_honesta_sem_projection() {
        assert!(context_summary_line(&ContextUsage::default())
            .contains("sem dados (projection ausente)"));
        let c = ContextUsage {
            window: 1_000_000,
            used: 250_000,
        };
        assert_eq!(context_summary_line(&c), "contexto: 250.0K/1.0M (25.0%)");
    }

    #[test]
    fn overlay_consome_qualquer_tecla_e_fecha() {
        let mut a = TuiApp::new("s", "w", false);
        a.overlay = Some(Overlay::Context);
        assert!(consume_key_for_overlay(&mut a), "tecla consumida pelo overlay");
        assert!(a.overlay.is_none(), "fecha com qualquer tecla");
        // Sem overlay aberto, NÃO consome (o fluxo normal de teclas segue).
        assert!(!consume_key_for_overlay(&mut a));
        a.overlay = Some(Overlay::Usage);
        assert!(consume_key_for_overlay(&mut a));
        assert!(a.overlay.is_none());
        a.overlay = Some(Overlay::Todos);
        assert!(consume_key_for_overlay(&mut a), "/todos fecha igual");
        assert!(a.overlay.is_none());
        // Buffer intocado (o handler nunca insere o char com overlay aberto).
        assert_eq!(a.input, "");
    }

    // ----- startup instantâneo (Fase V4-1): session_ready -----

    #[test]
    fn app_nasce_nao_pronto_e_mark_session_ready_destrava() {
        // Gate puro do handler: TuiApp::new default session_ready=false
        // (placeholder "" de sid até o 1º NewSession) e todos vazios.
        let mut a = TuiApp::new("", "w", false);
        assert!(!a.session_ready, "TUI aparece ANTES da sessão existir");
        assert!(a.todos.is_empty());
        assert!(!a.working);
        // mark_session_ready é a única via (apply do UiUpdate::NewSession).
        a.mark_session_ready();
        assert!(a.session_ready);
        // Idempotente (Ctrl+N em sessão pronta não mexe no estado).
        a.mark_session_ready();
        assert!(a.session_ready);
    }

    #[test]
    fn header_init_enquanto_sessao_nasce() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let pal = Theme::Dark.palette();
        let mut app = TuiApp::new("", "w", false);
        app.status = "conectando…".to_string();
        app.push_msg("system", &crate::ui::render::perm_unsupported_note());
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        let rows: Vec<String> = term
            .backend()
            .buffer()
            .content()
            .chunks(100)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        // Header honesto: state=INIT (nem working nem ready — a splash já
        // renderiza porque a arte não depende de sessão).
        assert!(rows.iter().any(|r| r.contains("state=INIT")), "{rows:?}");
        // Tela mínima: status compacto também sinaliza init + conectando
        // (largura inteira p/ a linha de status não truncar antes).
        let mut term_min = Terminal::new(TestBackend::new(100, 5)).unwrap();
        term_min.draw(|f| render(f, &mut app, &pal)).unwrap();
        let tela_min: String = term_min
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(tela_min.contains("init"), "{tela_min}");
        assert!(tela_min.contains("conectando"), "{tela_min}");
        // Sessão nasce → INIT sai, ready entra (mesmo app, mesmo terminal).
        app.mark_session_ready();
        app.session_id = "sess_pronta".into();
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        let rows2: Vec<String> = term
            .backend()
            .buffer()
            .content()
            .chunks(100)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        assert!(rows2.iter().any(|r| r.contains("state=ready")));
        assert!(!rows2.iter().any(|r| r.contains("state=INIT")));
        // DEAD continua dominando INIT (boot morreu antes de criar sessão).
        app.runtime_dead = true;
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        let rows3: Vec<String> = term
            .backend()
            .buffer()
            .content()
            .chunks(100)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        assert!(rows3.iter().any(|r| r.contains("state=DEAD")));
    }

    /// App de fixture p/ os testes de overlay: ctx 25% + usage completo.
    fn app_com_usage() -> TuiApp {
        let mut app = TuiApp::new("sess_overlay", "w", false);
        app.ctx = ContextUsage {
            window: 1_000_000,
            used: 250_000,
        };
        app.model = "zai/glm-5.3-Flash".to_string();
        app.last_usage = Some(Usage {
            total_tokens: 300_000,
            input_tokens: 100_000,
            output_tokens: 50_000,
            reasoning_tokens: 20_000,
            cache_creation_tokens: 30_000,
            cache_read_tokens: 100_000,
            model_request_count: 4,
        });
        app.last_stats = Some(TurnStats {
            input_tokens: 1_200,
            output_tokens: 800,
            total_tokens: 2_000,
            elapsed_ms: 4_250,
            tok_per_s: 188.2,
        });
        app
    }

    fn tela(term: &ratatui::Terminal<ratatui::backend::TestBackend>, w: usize) -> String {
        term.backend()
            .buffer()
            .content()
            .chunks(w)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn overlay_context_titulo_barra_legenda_rodape() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let pal = Theme::Dark.palette();
        let app = app_com_usage();
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| render_context_overlay(&app, f, f.area(), &pal))
            .unwrap();
        let screen = tela(&term, 100);
        // Título à esquerda + resumo à direita (mesma linha da borda).
        assert!(screen.contains("Context"));
        assert!(screen.contains("250.0K/1.0M (25.0%)"));
        // Barra de progresso: preenchimento + trilha na largura interna.
        assert!(screen.contains('█'), "barra preenchida com ctx>0");
        assert!(screen.contains('░'), "trilha da barra visível");
        // Legenda: 5 linhas (uma por tipo de token do protocolo).
        for nome in ["input", "output", "reasoning", "cache read", "cache creation"] {
            assert!(screen.contains(nome), "falta legenda {nome}");
        }
        // % sobre total_tokens (input 100K/300K) + valor absoluto curto.
        assert!(screen.contains("33.3%"));
        assert!(screen.contains("(100.0K)"));
        // Rodapé: cache hit = 100K/(100K+100K) = 50%.
        assert!(screen.contains("cache hit (aprox.): 50.0%"));
    }

    #[test]
    fn overlay_context_sem_projection_mensagem_honesta() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let pal = Theme::Dark.palette();
        let mut app = TuiApp::new("sess_semctx", "w", false); // ctx default = 0
        app.last_usage = None;
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| render_context_overlay(&app, f, f.area(), &pal))
            .unwrap();
        let screen = tela(&term, 100);
        assert!(
            screen.contains("sem dados de contexto (projection ausente)"),
            "mensagem honesta no lugar da barra zerada"
        );
        assert!(!screen.contains('█'), "sem barra mentirosa");
        assert!(!screen.contains('░'));
        assert!(screen.contains("cache hit (aprox.): 0.0%"), "sem dados → 0.0");
        // Legenda segue presente, com "—" em vez de zeros falsos.
        assert!(screen.contains("input"));
        assert!(screen.contains('—'));
    }

    #[test]
    fn overlay_usage_cartoes_e_placeholders() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let pal = Theme::Dark.palette();
        let app = app_com_usage();
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| render_usage_overlay(&app, f, f.area(), &pal))
            .unwrap();
        let screen = tela(&term, 100);
        // Dois cartões com labels claros + conteúdo formatado.
        assert!(screen.contains("Usage"));
        assert!(screen.contains("Session"));
        assert!(screen.contains("Last turn"));
        assert!(screen.contains("total  300.0K"));
        assert!(screen.contains("reqs   4"));
        assert!(screen.contains("model  zai/glm-5.3-Flash"));
        assert!(screen.contains("veloc  188.2 tok/s"));
        assert!(screen.contains("tempo  4.2s"));
        // Rodapé honesto: cota do plano não vem do protocolo.
        assert!(screen.contains("cota do plano (5h/semanal): não exposta pelo protocolo"));
        // Sem NENHUM dado: labels presentes, valores "—".
        let vazia = TuiApp::new("sess_vazia", "w", false);
        let mut term2 = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term2.draw(|f| render_usage_overlay(&vazia, f, f.area(), &pal))
            .unwrap();
        let screen2 = tela(&term2, 100);
        assert!(screen2.contains("Session"));
        assert!(screen2.contains("Last turn"));
        assert!(screen2.contains("—"), "placeholders claros sem dados");
        assert!(!screen2.contains("tok/s"));
    }

    #[test]
    fn overlay_nao_estoura_area_60x15_e_cobre_layout() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let pal = Theme::Dark.palette();
        for overlay in [Overlay::Context, Overlay::Usage, Overlay::Todos] {
            let mut app = app_com_usage();
            app.push_msg("user", "mensagem por baixo");
            app.overlay = Some(overlay);
            // 60x15: menor tela Full — overlay de 60% precisa caber sem panic.
            let mut term = Terminal::new(TestBackend::new(60, 15)).unwrap();
            term.draw(|f| render(f, &mut app, &pal)).unwrap();
            let screen = tela(&term, 60);
            let titulo = match overlay {
                Overlay::Context => "Context",
                Overlay::Usage => "Usage",
                Overlay::Todos => "Todos",
            };
            assert!(screen.contains(titulo), "{titulo} desenhado sobre o layout");
            // Transcript continua renderizado embaixo (write-back preservado).
            assert!(app.last_inner_height > 0, "scroll write-back intacto");
            assert!(app.overlay.is_some(), "render é puro: não fecha o overlay");
        }
    }

    #[test]
    fn overlay_todos_titulo_contagem_checkboxes_e_recorte() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let pal = Theme::Dark.palette();
        // Vazio: título SEM contagem + mensagem honesta (a mesma do REPL).
        let mut app = TuiApp::new("sess_todos", "w", false);
        app.overlay = Some(Overlay::Todos);
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| render_todos_overlay(&app, f, f.area(), &pal))
            .unwrap();
        let screen = tela(&term, 100);
        assert!(screen.contains("Todos"));
        assert!(screen.contains("sem todos registrados (o agente ainda não planejou)"));
        assert!(!screen.contains("/5"), "sem contagem quando vazio");

        // Populado (2/5): os três estados de checkbox + conteúdo.
        app.todos = vec![
            TodoItem { content: "ler plano".into(), status: TodoStatus::Completed },
            TodoItem { content: "testar tudo".into(), status: TodoStatus::Completed },
            TodoItem { content: "implementar feature".into(), status: TodoStatus::InProgress },
            TodoItem { content: "revisar docs".into(), status: TodoStatus::Pending },
            TodoItem { content: "publicar release".into(), status: TodoStatus::Pending },
        ];
        term.draw(|f| render_todos_overlay(&app, f, f.area(), &pal))
            .unwrap();
        let screen = tela(&term, 100);
        assert!(screen.contains("Todos"));
        assert!(screen.contains("2/5"), "contagem feitos/total à direita");
        assert!(screen.contains("[x] ler plano"));
        assert!(screen.contains("[~] implementar feature"));
        assert!(screen.contains("[ ] revisar docs"));
        // Em andamento em destaque: checkbox `[~]` na cor accent (pal.user) e
        // o CONTEÚDO do item com accent + BOLD (linha do `~`, células após o
        // checkbox).
        let buf = term.backend().buffer();
        let row_do_til = (0..buf.area.height)
            .find(|&y| (0..buf.area.width).any(|x| buf[(x, y)].symbol() == "~"))
            .expect("checkbox [~] presente");
        let linha: Vec<(usize, &ratatui::buffer::Cell)> = (0..buf.area.width as usize)
            .map(|x| (x, &buf[(x as u16, row_do_til)]))
            .collect();
        let pos = linha
            .iter()
            .position(|(_, c)| c.symbol() == "~")
            .unwrap();
        assert_eq!(linha[pos].1.fg, pal.user, "checkbox [~] na cor accent");
        assert!(
            linha[pos + 2..]
                .iter()
                .any(|(_, c)| c.fg == pal.user && c.modifier.contains(Modifier::BOLD)),
            "conteúdo do item em andamento em destaque (accent + BOLD)"
        );

        // Lista maior que a janela: corte honesto com contador (10 itens numa
        // janela de 10 linhas → 8 internas → 7 itens + "… +3 mais").
        let mut app8 = TuiApp::new("sess_todos8", "w", false);
        app8.todos = (0..10)
            .map(|i| TodoItem {
                content: format!("item {i}"),
                status: TodoStatus::Pending,
            })
            .collect();
        let mut term_peq = Terminal::new(TestBackend::new(60, 10)).unwrap();
        term_peq
            .draw(|f| render_todos_overlay(&app8, f, f.area(), &pal))
            .unwrap();
        let s8 = tela(&term_peq, 60);
        assert!(s8.contains("0/10"), "contagem cheia mesmo cortada");
        assert!(s8.contains("… +3 mais"), "contador de corte visível");
        assert!(!s8.contains("item 7"), "últimos itens ficam fora do recorte");
    }

    #[test]
    fn overlay_render_nao_muta_estado_do_app() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let pal = Theme::Dark.palette();
        let mut app = app_com_usage();
        app.input = "rascunho".into();
        app.cursor = app.input_len();
        let antes = (app.scroll, app.follow, app.input.clone(), app.cursor);
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| {
            render_context_overlay(&app, f, f.area(), &pal);
            render_usage_overlay(&app, f, f.area(), &pal);
        })
        .unwrap();
        assert_eq!(antes, (app.scroll, app.follow, app.input, app.cursor));
    }

    // ----- busca no transcript (Ctrl+F, Fase V4-2) -----

    fn app_com_historico_busca() -> TuiApp {
        let mut app = TuiApp::new("sess_busca", "w", false);
        app.push_msg("system", crate::ui::art::SPLASH_MARKER); // excluída
        app.push_msg("system", "boot notice");
        app.push_msg("user", "qual é a Capital da Austrália?");
        app.messages.push(ChatMsg {
            role: "assistant".into(),
            text: "hmm, deixa eu pensar\na capital é Canberra".into(),
            kind: MsgKind::Reasoning,
            wire: None,
        });
        app.push_msg("assistant", "A capital é Canberra. Fim.");
        app
    }

    #[test]
    fn busca_map_key_ctrlf_f3_shiftf3() {
        use KeyCode as K;
        use KeyModifiers as M;
        assert_eq!(map_key(key(K::Char('f'), M::CONTROL)), TuiKey::CtrlF);
        assert_eq!(map_key(key(K::F(3), M::empty())), TuiKey::F3);
        assert_eq!(map_key(key(K::F(3), M::SHIFT)), TuiKey::ShiftF3);
        // CONTROL/ALT + F3 não são busca (caem no Ignore).
        assert_eq!(map_key(key(K::F(3), M::CONTROL)), TuiKey::Ignore);
        // 'f' simples continua digitando no buffer.
        assert_eq!(map_key(key(K::Char('f'), M::empty())), TuiKey::Char('f'));
    }

    #[test]
    fn busca_find_matches_case_reasoning_splash_e_vazia() {
        let app = app_com_historico_busca();
        // Case-insensitive: "capital" bate user + assistant (2 msgs), não o
        // reasoning (que só tem "a capital é Canberra"... espera: TEM).
        let ms = find_matches(&app.messages, "CAPITAL");
        // user "qual é a Capital..." + reasoning "a capital é Canberra" +
        // assistant "A capital é Canberra." = 3 matches (splash fora).
        assert_eq!(ms.len(), 3);
        assert_eq!(ms[0].msg_idx, 2, "ordem do histórico preservada");
        assert_eq!(ms[0].role, "USER");
        assert_eq!(ms[1].role, "THINK", "reasoning incluído com badge próprio");
        assert_eq!(ms[2].role, "ASSIST");
        // Preview: primeira linha COM o match (não a 1ª da mensagem).
        assert_eq!(ms[1].preview, "a capital é Canberra");
        // Splash-marker nunca vira match (busca pelo texto do marcador).
        assert!(find_matches(&app.messages, "zcode-splash").is_empty());
        // Query vazia/em branco → 0 resultados (digitação inicial não lista tudo).
        assert!(find_matches(&app.messages, "").is_empty());
        assert!(find_matches(&app.messages, "   ").is_empty());
        // Sem correpondência → 0.
        assert!(find_matches(&app.messages, "zzz-inexistente").is_empty());
    }

    #[test]
    fn busca_preview_truncado_e_linha_vazia() {
        let mut app = TuiApp::new("s", "w", false);
        let longo: String = "x".repeat(200);
        app.push_msg("assistant", &longo);
        let ms = find_matches(&app.messages, "x");
        assert_eq!(ms.len(), 1);
        assert!(cell_width(&ms[0].preview) <= 60, "preview ~60 células");
        assert!(ms[0].preview.ends_with("..."), "truncado com marcador");
        // Mensagem multilinha com o match numa linha do meio: preview é a
        // linha do match; match na 1ª linha não-vazia funciona como fallback.
        app.push_msg("user", "\n\nmeio com alvo\ntambém alvo aqui");
        let ms2 = find_matches(&app.messages, "alvo");
        assert_eq!(ms2.len(), 1);
        assert_eq!(ms2[0].preview, "meio com alvo");
    }

    #[test]
    fn busca_jump_offset_clampa_no_topo() {
        // Meia tela acima do início da mensagem; nunca negativo (satura em 0).
        assert_eq!(jump_offset(0, 20), 0);
        assert_eq!(jump_offset(5, 20), 0, "prefixo < meia tela → topo");
        assert_eq!(jump_offset(40, 20), 30);
        assert_eq!(jump_offset(u16::MAX, 10), u16::MAX - 5, "satura sem overflow");
        // Viewport zero (nunca renderizou): offset = prefixo (render clampa).
        assert_eq!(jump_offset(100, 0), 100);
    }

    #[test]
    fn busca_transcript_prefix_lines_medida_do_prefixo() {
        use crate::ui::theme::Theme;
        let pal = Theme::Dark.palette();
        let mut app = app_com_historico_busca();
        // Largura 80 (sem wrap): notice=1 · user=1 · sep=1 · think=2 · sep=1
        // · assistant=1. Separador só ENTRE mensagens de conversa (não antes
        // da 1ª); prefixo vazio/só-splash → 0 (placeholder não conta — o
        // transcript real não o renderiza com histórico não-vazio).
        assert_eq!(transcript_prefix_lines(&mut app, &pal, 80, 0), 0);
        assert_eq!(transcript_prefix_lines(&mut app, &pal, 80, 1), 0, "só a splash");
        assert_eq!(transcript_prefix_lines(&mut app, &pal, 80, 2), 1, "notice");
        assert_eq!(transcript_prefix_lines(&mut app, &pal, 80, 3), 2, "notice+user");
        assert_eq!(transcript_prefix_lines(&mut app, &pal, 80, 4), 5, "+sep+think (2 linhas)");
        assert_eq!(transcript_prefix_lines(&mut app, &pal, 80, 5), 7, "+sep+assistant (histórico todo)");
        assert_eq!(transcript_prefix_lines(&mut app, &pal, 80, 999), 7, "clamp no comprimento");
        // Largura zero: sem medição (área inválida).
        assert_eq!(transcript_prefix_lines(&mut app, &pal, 0, 3), 0);
        // Medição NÃO suja o histórico (versão/cache intactos).
        let rev = app.messages_rev;
        assert_eq!(app.messages.len(), 5);
        assert_eq!(app.messages_rev, rev);
    }

    #[test]
    fn busca_estado_abrir_cancelar_restaura_buffer() {
        let mut app = app_com_historico_busca();
        app.input = "rascunho caro".into();
        app.cursor = 8;
        // Ctrl+F: guarda o buffer, esvazia o input, linha aberta.
        app.open_search();
        assert!(app.search.as_ref().is_some_and(|s| s.open));
        assert_eq!(app.input, "");
        // Digitação edita a query e recalcula matches a cada tecla.
        for c in "capital".chars() {
            app.search_insert_char(c);
        }
        let s = app.search.as_ref().unwrap();
        assert_eq!(s.query, "capital");
        assert_eq!(s.matches.len(), 3);
        assert_eq!(s.cursor, 0);
        // Esc: cancela — buffer restaurado INTEGRO (busca nunca destrói o
        // texto do usuário) e busca descartada.
        app.cancel_search();
        assert_eq!(app.input, "rascunho caro");
        assert_eq!(app.cursor, 8);
        assert!(app.search.is_none());
    }

    #[test]
    fn busca_estado_enter_confirma_sticky_e_f3() {
        let mut app = app_com_historico_busca();
        app.input = "rascunho".into();
        app.cursor = app.input_len();
        app.last_inner_width = 80;
        app.last_inner_height = 10;
        app.open_search();
        for c in "capital".chars() {
            app.search_insert_char(c);
        }
        // Enter: fecha (sticky), restaura o buffer e SALTA pro match atual.
        app.search_confirm_jump();
        assert!(!app.search.as_ref().unwrap().open, "linha fechada");
        assert_eq!(app.input, "rascunho", "buffer restaurado ao confirmar");
        assert!(!app.follow, "salto desliga o follow");
        assert_eq!(app.scroll, 0, "match 1 (msg 2) com viewport 10: topo");
        // F3 (sticky): próximo match — cursor avança e o offset segue o
        // prefixo medido da mensagem.
        app.search_step(1);
        let s = app.search.as_ref().unwrap();
        assert_eq!(s.cursor, 1);
        assert_eq!(app.scroll, jump_offset(3, 10), "prefixo do think (3) − meia tela");
        // Shift+F3 volta; nos extremos fica parado (sem wrap).
        app.search_step(-1);
        assert_eq!(app.search.as_ref().unwrap().cursor, 0);
        app.search_step(-1);
        assert_eq!(app.search.as_ref().unwrap().cursor, 0, "sem wrap no 1º");
        app.search_step(1);
        app.search_step(1);
        app.search_step(1);
        assert_eq!(app.search.as_ref().unwrap().cursor, 2, "sem wrap no último");
        // Ctrl+F reabre a sticky preservando a query (cursor no fim).
        app.open_search();
        let s = app.search.as_ref().unwrap();
        assert!(s.open);
        assert_eq!(s.query, "capital");
        assert_eq!(s.qcursor, s.query.chars().count());
        assert_eq!(app.input, ""); // buffer guardado de novo
    }

    #[test]
    fn busca_edicao_query_cursor_multibyte() {
        let mut app = TuiApp::new("s", "w", false);
        app.push_msg("user", "procurar 界😀 aqui");
        app.open_search();
        app.search_insert_str("界😀");
        {
            let s = app.search.as_ref().unwrap();
            assert_eq!(s.query, "界😀");
            assert_eq!(s.qcursor, 2, "cursor em CHARs, não bytes");
            assert_eq!(s.matches.len(), 1);
        }
        // Editar no MEIO (antes do emoji) não quebra fronteira.
        app.search_move_home();
        app.search_insert_char('x');
        assert_eq!(app.search.as_ref().unwrap().query, "x界😀");
        app.search_backspace();
        assert_eq!(app.search.as_ref().unwrap().query, "界😀");
        // Delete apusa à frente do cursor.
        app.search_move_home();
        app.search_delete();
        assert_eq!(app.search.as_ref().unwrap().query, "😀");
        // Backspace no 0 e Delete no fim são no-ops.
        app.search_move_home();
        app.search_backspace();
        app.search_move_end();
        app.search_delete();
        assert_eq!(app.search.as_ref().unwrap().query, "😀");
        // Left/Right saturam nas pontas.
        app.search_move_left();
        app.search_move_left();
        app.search_move_left();
        assert_eq!(app.search.as_ref().unwrap().qcursor, 0);
        app.search_move_right();
        app.search_move_right();
        app.search_move_right();
        assert_eq!(app.search.as_ref().unwrap().qcursor, 1);
        // Query que zera os matches → status honesto.
        app.search_insert_char('z');
        assert!(app.search.as_ref().unwrap().matches.is_empty());
        assert_eq!(search_status(app.search.as_ref().unwrap()), "sem resultados");
    }

    #[test]
    fn busca_status_contagem() {
        let mut s = SearchState {
            query: "a".into(),
            matches: vec![
                SearchMatch { msg_idx: 0, preview: String::new(), role: "USER".into() },
                SearchMatch { msg_idx: 3, preview: String::new(), role: "ASSIST".into() },
            ],
            cursor: 1,
            qcursor: 1,
            open: true,
            saved_input: String::new(),
            saved_cursor: 0,
        };
        assert_eq!(search_status(&s), "match 2/2");
        s.cursor = 0;
        assert_eq!(search_status(&s), "match 1/2");
        s.matches.clear();
        assert_eq!(search_status(&s), "sem resultados");
    }

    #[test]
    fn busca_render_linha_find_no_testbackend() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let pal = Theme::Dark.palette();
        let mut app = app_com_historico_busca();
        app.mark_session_ready();
        app.input = "digitando".into();
        app.cursor = app.input_len();
        // Draw ANTES: input normal visível.
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        let tela_input = tela(&term, 100);
        assert!(tela_input.contains("digitando"));
        assert!(tela_input.contains("input:"));
        // Abre a busca e digita: o prompt vira a linha find: com status.
        app.open_search();
        for c in "capital".chars() {
            app.search_insert_char(c);
        }
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        let tela_busca = tela(&term, 100);
        assert!(tela_busca.contains("find:"), "prefixo find: no lugar do input");
        assert!(tela_busca.contains("capital"), "query visível");
        assert!(tela_busca.contains("match 1/3"), "status do match ao lado");
        assert!(tela_busca.contains("find: Enter salta"), "título ensina as teclas");
        assert!(!tela_busca.contains("digitando"), "buffer guardado não vaza");
        // Confirmar restaura o input no próximo draw.
        app.search_confirm_jump();
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        assert!(tela(&term, 100).contains("digitando"));
        // Tela mínima também mostra a linha find:.
        let mut term_min = Terminal::new(TestBackend::new(100, 4)).unwrap();
        let mut app2 = app_com_historico_busca();
        app2.open_search();
        app2.search_insert_char('c');
        term_min.draw(|f| render(f, &mut app2, &pal)).unwrap();
        let tela_min = tela(&term_min, 100);
        assert!(tela_min.contains("find:"));
        assert!(tela_min.contains("c"));
        assert!(tela_min.contains("match 1/"));
    }

    #[test]
    fn busca_salto_usa_largura_do_ultimo_render() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let pal = Theme::Dark.palette();
        let mut app = TuiApp::new("sess_jump", "w", false);
        // Histórico longo: cada mensagem rende várias linhas visuais.
        for i in 0..30 {
            app.push_msg("assistant", &format!("msg {i}: {}", "palavra ".repeat(20)));
        }
        app.open_search();
        app.search_insert_str("msg 25");
        assert_eq!(app.search.as_ref().unwrap().matches.len(), 1);
        // Render real grava largura/altura internas (write-back gêmeo).
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        assert!(app.last_inner_width > 0 && app.last_inner_height > 0);
        // Confirma com a linha AINDA aberta (Enter do handler): salto mede o
        // prefixo na largura real → offset > 0 (match profundo no histórico).
        app.search_confirm_jump();
        assert!(!app.follow);
        assert!(app.scroll > 0, "match 26/30 com viewport curta salta p/ dentro");
        // Draw de novo: offset clampado sobrevive (o prefixo existe mesmo com
        // a linha find: fechada — busca sticky não quebra o render).
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        assert!(app.scroll > 0);
    }

    // ----- fila de mensagens (Fase V5-1) -----

    #[test]
    fn fila_push_pop_fifo_e_gate_vazia() {
        let mut a = TuiApp::new("s", "w", false);
        // Gate do consumo: fila vazia → None (nada a iniciar).
        assert_eq!(a.next_queued(), None);
        // Múltiplas linhas permitidas; ordem FIFO preservada.
        a.queue_push("primeira");
        a.queue_push("segunda\ncom duas linhas");
        a.queue_push("terceira");
        assert_eq!(a.queue.len(), 3);
        assert_eq!(a.next_queued().as_deref(), Some("primeira"));
        assert_eq!(a.next_queued().as_deref(), Some("segunda\ncom duas linhas"));
        assert_eq!(a.next_queued().as_deref(), Some("terceira"));
        assert_eq!(a.next_queued(), None, "fila esvaziada → None de novo");
    }

    #[test]
    fn fila_clear_descarta_tudo_e_conta() {
        let mut a = TuiApp::new("s", "w", false);
        assert_eq!(a.queue_clear(), 0, "clear em fila vazia é no-op contável");
        a.queue_push("a");
        a.queue_push("b");
        assert_eq!(a.queue_clear(), 2);
        assert!(a.queue.is_empty());
        assert_eq!(a.next_queued(), None);
    }

    #[test]
    fn fila_badge_formatacao_pura() {
        // Badge do prompt/spinner: vazio sem fila, prefixado com espaço.
        // B-1: o enfileirar NÃO grava status "na fila: N" — o badge é a
        // única indicação (o status segue "working…" durante o turno).
        assert_eq!(queue_badge(0), "");
        assert_eq!(queue_badge(3), " [fila: 3]");
        let mut a = TuiApp::new("s", "w", false);
        a.queue_push("x");
        a.queue_push("y");
        a.status = "working…".to_string();
        assert_eq!(a.status, "working…", "enfileirar não troca o status");
    }

    #[test]
    fn fila_indicador_no_render_prompt_e_spinner() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let pal = Theme::Dark.palette();
        let mut app = TuiApp::new("sess_fila", "w", false);
        app.mark_session_ready();
        app.push_msg("user", "pergunta em andamento");
        app.working = true;
        app.queue_push("mensagem enfileirada 1");
        app.queue_push("mensagem 2");
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        let screen = tela(&term, 100);
        // Badge sutil no título do prompt (durante working)…
        assert!(screen.contains("[fila: 2]"), "{screen}");
        // …e na linha de spinner do transcript.
        assert!(screen.contains("working… (Esc cancela) [fila: 2]"), "{screen}");
        // Fila consumida: badge some do próximo draw.
        app.next_queued();
        app.next_queued();
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        assert!(!tela(&term, 100).contains("[fila:"));
        // Tela mínima: o badge acompanha a linha de status compacta.
        let mut app2 = TuiApp::new("sess_fila2", "w", false);
        app2.queue_push("z");
        let mut term_min = Terminal::new(TestBackend::new(100, 5)).unwrap();
        term_min.draw(|f| render(f, &mut app2, &pal)).unwrap();
        assert!(tela(&term_min, 100).contains("[fila: 1]"));
    }

    // ----- highlight dos fences (syntect, Fase V5-2) -----

    #[test]
    fn highlight_fence_rust_cores_da_paleta_e_1_para_1_no_historico() {
        use crate::ui::theme::Theme;
        use std::collections::HashSet;
        let pal = Theme::Dark.palette();
        let msg = "antes
```rust
let s = \"oi\";
let n = 7;
```
depois";
        let mut app = TuiApp::new("sess_hl", "w", false);
        app.push_msg("assistant", msg);
        let lines = history_lines(&app, &pal, 80, u16::MAX, false);
        // Invariante 1:1 ABSOLUTO: highlight acontece DEPOIS da divisão em
        // linhas — cada linha de entrada segue virando exatamente UMA Line.
        assert_eq!(lines.len(), msg.lines().count());
        let texto = |l: &Line| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        // Linhas de código recompostas (o prefixo "   " da continuação entra;
        // a 1ª linha da mensagem carrega o badge, então o código vem depois).
        assert_eq!(texto(&lines[2]).trim(), "let s = \"oi\";");
        assert_eq!(texto(&lines[3]).trim(), "let n = 7;");
        // Cores da NOSSA paleta: keyword → prompt; string → system; número →
        // badge_fg. ≥3 fg distintos no bloco = highlight de verdade (o corpo
        // cru tinha SEMPRE fg nenhum).
        for l in [&lines[2], &lines[3]] {
            let trechos = l.spans.iter().filter(|s| s.style.bg.is_some());
            assert!(trechos.clone().all(|s| s.style.bg == Some(pal.code_bg)));
        }
        let mut fgs: HashSet<_> = HashSet::new();
        for l in [&lines[2], &lines[3]] {
            for s in &l.spans {
                if let Some(fg) = s.style.fg {
                    fgs.insert(fg);
                }
            }
        }
        assert!(fgs.len() >= 3, "≥3 cores da paleta no bloco: {fgs:?}");
        assert!(fgs.contains(&pal.prompt), "keyword em prompt: {fgs:?}");
        assert!(fgs.contains(&pal.system), "string em system: {fgs:?}");
        // Linhas fora do fence: SEM fg de highlight (comportamento de sempre).
        assert!(lines[0].spans.iter().all(|s| s.style.fg != Some(pal.prompt)));
        assert!(lines[5].spans.iter().all(|s| s.style.fg != Some(pal.system)));
    }

    #[test]
    fn highlight_fences_no_transcript_medicao_bate_com_o_render() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let pal = Theme::Dark.palette();
        let msg = "plano:
```rust
fn main() {
    let msg = \"olá mundo\";
    println!(\"{}\", msg);
}
```
fim";
        let mut app = TuiApp::new("sess_hl2", "w", false);
        app.push_msg("assistant", msg);
        // 1:1 no nível do histórico.
        let lines = history_lines(&app, &pal, 80, u16::MAX, false);
        assert_eq!(lines.len(), msg.lines().count());
        // E a medição de scroll continua batendo com o render real
        // (highlight com estado multiline NÃO pode mudar o wrap).
        let text = Text::from(lines);
        let largura = 40u16;
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
        // Render completo da TUI com o fence highlightado: sem panic e o
        // código aparece (bg do código presente no buffer não é verificável
        // por célula aqui — o assert é o draw limpo + contagem acima).
        let mut app2 = TuiApp::new("sess_hl2", "w", false);
        app2.mark_session_ready();
        app2.push_msg("assistant", msg);
        let mut term2 = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term2.draw(|f| render(f, &mut app2, &pal)).unwrap();
    }

    // ----- picker de sessões (/resume, Fase V5-2) -----

    fn rows_fixas() -> Vec<SessionRow> {
        (0..3)
            .map(|i| SessionRow {
                id: format!("sess_{i}"),
                title: format!("sessão {i}"),
                status: "active".into(),
                created: "2026-09-08".into(),
            })
            .collect()
    }

    #[test]
    fn picker_estado_loading_fill_vazio_e_selected_id() {
        let mut a = TuiApp::new("s", "w", false);
        assert!(a.picker.is_none());
        a.open_session_picker();
        assert_eq!(a.status, "buscando sessões…");
        let p = a.picker.as_ref().unwrap();
        assert_eq!(p.data, PickerData::Loading);
        assert!(p.is_empty());
        assert_eq!(p.selected_id(), None, "loading não seleciona nada");
        // Resultado vazio → estado honesto, sem itens.
        a.fill_session_picker(vec![]);
        assert_eq!(
            a.picker.as_ref().unwrap().data,
            PickerData::Empty,
            "vazio → estado próprio (nenhuma sessão encontrada)"
        );
        // Lista pronta: seleção volta ao 1º item.
        a.fill_session_picker(rows_fixas());
        let p = a.picker.as_ref().unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p.selected, 0);
        assert_eq!(p.selected_id().as_deref(), Some("sess_0"));
        assert_eq!(
            a.status,
            "sessões — ↑/↓ navega · Enter retoma · Esc fecha"
        );
    }

    #[test]
    fn picker_fill_chegou_depois_do_esc_e_descartado() {
        let mut a = TuiApp::new("s", "w", false);
        a.open_session_picker();
        a.close_session_picker();
        assert!(a.picker.is_none());
        assert_eq!(a.status, "pronto", "Esc devolve o status ao repouso");
        // Fetch chegou depois do fechamento: descarte silencioso (nada
        // reabre o picker por conta própria).
        a.fill_session_picker(rows_fixas());
        assert!(a.picker.is_none());
    }

    #[test]
    fn picker_navegacao_clamp_sem_wrap_e_pagina() {
        // CLAMP (documentado no SessionPicker): nos extremos fica parado,
        // mesma decisão da navegação de matches da busca (menos surpresa).
        let mut a = TuiApp::new("s", "w", false);
        a.open_session_picker();
        a.fill_session_picker(rows_fixas());
        let p = a.picker.as_mut().unwrap();
        p.move_selected(-1); // ↑ no topo: fica
        assert_eq!(p.selected, 0);
        p.move_selected(1);
        p.move_selected(1);
        assert_eq!(p.selected, 2);
        p.move_selected(1); // ↓ no fim: fica
        assert_eq!(p.selected, 2);
        // Página fixa de 10 (PICKER_PAGE_STEP), clampada.
        p.move_selected(PICKER_PAGE_STEP);
        assert_eq!(p.selected, 2, "página além do fim clampa");
        // Lista com 25 itens: página anda de verdade.
        let muitas: Vec<SessionRow> = (0..25)
            .map(|i| SessionRow {
                id: format!("sess_m{i}"),
                title: format!("m{i}"),
                status: "active".into(),
                created: "2026-09-08".into(),
            })
            .collect();
        a.fill_session_picker(muitas);
        let p = a.picker.as_mut().unwrap();
        p.move_selected(PICKER_PAGE_STEP);
        assert_eq!(p.selected, 10);
        p.move_selected(-PICKER_PAGE_STEP);
        assert_eq!(p.selected, 0);
        p.move_selected(-PICKER_PAGE_STEP);
        assert_eq!(p.selected, 0, "página antes do topo clampa");
        // Loading/Empty: navegação é no-op.
        a.open_session_picker();
        a.picker.as_mut().unwrap().move_selected(1);
        assert_eq!(a.picker.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn picker_take_selected_fecha_e_devolve_o_id() {
        let mut a = TuiApp::new("s", "w", false);
        a.open_session_picker();
        assert_eq!(a.take_selected_session(), None, "loading: nada p/ tomar");
        assert!(a.picker.is_some(), "sem escolha, o picker permanece aberto");
        a.fill_session_picker(rows_fixas());
        a.picker.as_mut().unwrap().move_selected(2);
        assert_eq!(
            a.take_selected_session().as_deref(),
            Some("sess_2"),
            "Enter toma o id da linha selecionada"
        );
        assert!(a.picker.is_none(), "Enter fecha o picker");
    }

    #[test]
    fn picker_window_recorte_contem_a_selecao() {
        // Cabe inteiro → tudo; não cabe → janela deslizante com a seleção
        // dentro (âncora no fim ao descer, no topo ao subir); degenerados.
        assert_eq!(picker_window(0, 0, 5), (0, 0));
        assert_eq!(picker_window(3, 2, 5), (0, 3), "cabe → lista inteira");
        assert_eq!(picker_window(25, 0, 10), (0, 10));
        assert_eq!(picker_window(25, 9, 10), (0, 10), "9 < 10: ainda tudo");
        assert_eq!(picker_window(25, 10, 10), (1, 11), "10 vira âncora no fim");
        assert_eq!(picker_window(25, 24, 10), (15, 25), "fim da lista");
        let (s, e) = picker_window(25, 12, 10);
        assert!(s <= 12 && 12 < e, "seleção sempre visível: {s}..{e}");
    }

    #[test]
    fn picker_row_line_colunas_truncamento_e_selecao() {
        use crate::ui::theme::Theme;
        let pal = Theme::Dark.palette();
        let texto = |l: &Line| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        let row = SessionRow {
            id: "sess_abcdef123456".into(),
            title: "título bem comprido que precisa ser truncado para caber na coluna".into(),
            status: "active".into(),
            created: "2026-09-08T10:00:00Z".into(),
        };
        let sel = picker_row_line(&row, true, 60, &pal);
        let nao = picker_row_line(&row, false, 60, &pal);
        // Colunas presentes e truncadas (data cortada em 10, status em 8).
        for l in [&sel, &nao] {
            let t = texto(l);
            assert!(t.contains("2026-09-08"), "data truncada: {t}");
            assert!(t.contains("active"), "status: {t}");
            assert!(t.contains("sess_abc"), "id curto: {t}");
            assert!(cell_width(&t) <= 60, "linha cabe na largura: {t}");
        }
        // Selecionada: prefixo "> " e título em accent+BOLD; não selecionada:
        // "  " e título na cor assistant.
        assert!(texto(&sel).starts_with("> "));
        assert!(texto(&nao).starts_with("  "));
        let tit_sel = sel.spans.iter().find(|s| s.content.contains("título")).unwrap();
        assert_eq!(tit_sel.style.fg, Some(pal.user));
        assert!(tit_sel.style.add_modifier.contains(Modifier::BOLD));
        let tit_nao = nao.spans.iter().find(|s| s.content.contains("título")).unwrap();
        assert_eq!(tit_nao.style.fg, Some(pal.assistant));
        // Título é truncado com marcador (suffix ASCII "..." de
        // `truncate_cells` — 3 pontos, não o char ellipsis).
        assert!(texto(&sel).contains("..."), "{:?}", texto(&sel));
        assert!(cell_width(&texto(&sel)) <= 60);
    }

    #[test]
    fn picker_render_estados_no_testbackend() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let pal = Theme::Dark.palette();
        // Loading: status honesto visível.
        let mut app = TuiApp::new("s", "w", false);
        app.open_session_picker();
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        let screen = tela(&term, 100);
        assert!(screen.contains("Sessões"), "{screen}");
        assert!(screen.contains("buscando sessões…"), "{screen}");
        assert!(screen.contains("↑/↓ navega"), "dica na base: {screen}");
        // Vazio: mensagem honesta.
        app.fill_session_picker(vec![]);
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        assert!(
            tela(&term, 100).contains("nenhuma sessão encontrada"),
            "{}",
            tela(&term, 100)
        );
        // Itens: título, contador sel/total e marca da seleção.
        app.fill_session_picker(rows_fixas());
        app.picker.as_mut().unwrap().move_selected(1);
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        let screen = tela(&term, 100);
        assert!(screen.contains("sessão 0"), "{screen}");
        assert!(screen.contains("sessão 1"), "{screen}");
        assert!(screen.contains("2/3"), "contador sel/total: {screen}");
        // O render NÃO muta o estado (a linha de baixo confirma seleção viva).
        assert_eq!(app.picker.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn picker_render_lista_longa_recorta_com_janela_deslizante() {
        use crate::ui::theme::Theme;
        use ratatui::{backend::TestBackend, Terminal};
        let pal = Theme::Dark.palette();
        let mut app = TuiApp::new("s", "w", false);
        app.open_session_picker();
        let muitas: Vec<SessionRow> = (0..30)
            .map(|i| SessionRow {
                id: format!("sess_m{i:02}"),
                title: format!("título {i:02}"),
                status: "active".into(),
                created: "2026-09-08".into(),
            })
            .collect();
        app.fill_session_picker(muitas);
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        // Seleção no topo: primeiras linhas visíveis, as de fora NÃO.
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        let screen = tela(&term, 100);
        assert!(screen.contains("título 00"), "{screen}");
        assert!(!screen.contains("título 29"), "recorte: {screen}");
        assert!(screen.contains("1/30"), "indicador do recorte: {screen}");
        // Página de seleção para o fim: janela desliza, seleção visível.
        app.picker
            .as_mut()
            .unwrap()
            .move_selected(3 * PICKER_PAGE_STEP);
        term.draw(|f| render(f, &mut app, &pal)).unwrap();
        let screen = tela(&term, 100);
        assert!(screen.contains("título 29"), "seleção visível: {screen}");
        assert!(!screen.contains("título 00 "), "topo saiu da janela: {screen}");
        assert!(screen.contains("30/30"), "contador sel+1/total: {screen}");
    }
}
