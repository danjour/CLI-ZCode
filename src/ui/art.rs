//! Arte half-block truecolor (formato FMT1) embutida no binário.
//!
//! Formato dos assets: a 1ª linha é `FMT1 <cols>x<rows>`; cada linha seguinte
//! tem exatamente `<cols>` tokens separados por UM espaço. Cada token é
//! `▀#RRGGBB/#RRGGBB` — fg = cor da metade de cima da célula, bg = cor da
//! metade de baixo. Como o glifo `▀` cobre 2 px verticais, a grade
//! `<cols>×<rows>` representa `<cols>×(<rows>*2)` pixels.
//!
//! O parser é tolerante: token malformado é pulado sem panic; célula faltante
//! (arquivo truncado) amostra como preto. A conversão para o terminal é
//! nearest-sampling determinístico com cache de 1 entrada por arte (o texto
//! só muda quando muda a largura/altura pedida).

use std::sync::{Mutex, OnceLock};

use ratatui::{
    style::{Color, Style},
    text::{Line, Span, Text},
};

use crate::ui::theme;

/// Olho anime completo (janela retrô + íris dourada + pupila coração):
/// 96 colunas × 36 linhas de células (grade 96×72 px).
pub const EYE_ART: &str = include_str!("../../assets/eye-art-96.txt");

/// Olho compacto p/ sidebar e saída texto (REPL/headless): 26 colunas × 12
/// linhas (26×24 px).
pub const EYE_ART_SIDE: &str = include_str!("../../assets/eye-art-side.txt");

/// Mensagem-marcador da splash no histórico da TUI (role "system"): o
/// renderer detecta este texto exato e desenha a arte half-block no lugar.
pub const SPLASH_MARKER: &str = "zcode-splash-marker";

const EYE_COLS: usize = 96;
const EYE_ROWS: usize = 36;
const SIDE_COLS: usize = 26;
const SIDE_ROWS: usize = 12;

/// Cor 24-bit.
pub type Rgb = (u8, u8, u8);

/// Célula half-block: (cor da metade de cima, cor da metade de baixo).
type Cell = (Rgb, Rgb);

/// Grade FMT1 já parseada (células em ordem row-major).
pub struct ArtGrid {
    pub cols: usize,
    pub rows: usize,
    cells: Vec<Cell>,
}

/// Converte `#RRGGBB` em RGB; qualquer desvio → None (token é pulado).
fn parse_hex(token: &str) -> Option<Rgb> {
    if token.len() != 7 || !token.starts_with('#') {
        return None;
    }
    let byte = |range: std::ops::Range<usize>| -> Option<u8> {
        u8::from_str_radix(token.get(range)?, 16).ok()
    };
    Some((byte(1..3)?, byte(3..5)?, byte(5..7)?))
}

/// Parser FMT1 tolerante: cabeçalho inválido → grade vazia; token malformado
/// é pulado sem panic; linha curta simplesmente deixa células a menos (o
/// acesso via `cell` trata a falha como preto).
pub fn parse_fmt1(source: &str) -> ArtGrid {
    let mut lines = source.lines();
    let dims = lines
        .next()
        .and_then(|header| header.trim().strip_prefix("FMT1 "))
        .and_then(|rest| {
            let mut parts = rest.split('x');
            let cols = parts.next()?.trim().parse::<usize>().ok()?;
            let rows = parts.next()?.trim().parse::<usize>().ok()?;
            Some((cols, rows))
        });
    let Some((cols, rows)) = dims else {
        return ArtGrid {
            cols: 0,
            rows: 0,
            cells: Vec::new(),
        };
    };
    let mut cells = Vec::with_capacity(cols * rows);
    for line in lines {
        for token in line.split(' ') {
            let mut chars = token.chars();
            if chars.next() != Some('▀') {
                continue; // token malformado: pula sem panic
            }
            let mut halves = chars.as_str().split('/');
            let top = halves.next().and_then(parse_hex);
            let bottom = halves.next().and_then(parse_hex);
            if let (Some(top), Some(bottom)) = (top, bottom) {
                cells.push((top, bottom));
            }
        }
    }
    ArtGrid { cols, rows, cells }
}

impl ArtGrid {
    /// Célula amostrada; índice fora da faixa (arquivo truncado) → preto.
    fn cell(&self, row: usize, col: usize) -> Cell {
        self.cells
            .get(row * self.cols + col)
            .copied()
            .unwrap_or(((0, 0, 0), (0, 0, 0)))
    }
}

fn eye_grid() -> &'static ArtGrid {
    static GRID: OnceLock<ArtGrid> = OnceLock::new();
    GRID.get_or_init(|| parse_fmt1(EYE_ART))
}

fn side_grid() -> &'static ArtGrid {
    static GRID: OnceLock<ArtGrid> = OnceLock::new();
    GRID.get_or_init(|| parse_fmt1(EYE_ART_SIDE))
}

/// Amostragem nearest pelo centro do bloco (mesma regra determinística do
/// renderer de texto antigo): posição de saída `position` em alvo `target`
/// escolhe a célula de origem correspondente.
fn sample_index(position: usize, target: usize, source_len: usize) -> usize {
    if target == 0 || source_len == 0 {
        return 0;
    }
    (((position * 2 + 1) * source_len) / (target * 2)).min(source_len - 1)
}

/// Recorte central vertical: primeira linha de origem quando só cabem `rows`
/// das `grid_rows` linhas.
fn crop_start(grid_rows: usize, rows: usize) -> usize {
    grid_rows.saturating_sub(rows) / 2
}

/// Converte a célula em `Style`; `indexed` degrada para 256 cores
/// (`Color::Indexed`) quando o terminal não anuncia truecolor.
fn cell_style(cell: Cell, indexed: bool) -> Style {
    let (top, bottom) = cell;
    if indexed {
        Style::default()
            .fg(Color::Indexed(rgb_to_256(top)))
            .bg(Color::Indexed(rgb_to_256(bottom)))
    } else {
        Style::default()
            .fg(Color::Rgb(top.0, top.1, top.2))
            .bg(Color::Rgb(bottom.0, bottom.1, bottom.2))
    }
}

/// Grade → `Text`: cada `Span` é um run de células consecutivas de mesma cor
/// (a arte tem muitas repetições), todas com o glifo `▀` — fg+cima, bg+baixo.
fn grid_text(grid: &ArtGrid, cols: usize, rows: usize, indexed: bool) -> Text<'static> {
    if cols == 0 || rows == 0 {
        return Text::default();
    }
    let top = crop_start(grid.rows, rows);
    let mut lines = Vec::with_capacity(rows);
    for y in 0..rows {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut run = String::new();
        let mut current: Option<Cell> = None;
        for x in 0..cols {
            let cell = grid.cell(top + y, sample_index(x, cols, grid.cols));
            if current != Some(cell) && !run.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut run),
                    cell_style(current.unwrap(), indexed),
                ));
            }
            current = Some(cell);
            run.push('▀');
        }
        if let Some(cell) = current {
            spans.push(Span::styled(run, cell_style(cell, indexed)));
        }
        lines.push(Line::from(spans));
    }
    Text::from(lines)
}

/// Cache de 1 entrada por arte: reconstruir o `Text` a cada draw seria
/// desperdício porque a chave (arte, indexed, cols, rows) muda raramente.
struct TextCache {
    key: (u8, u16, u16, u16),
    text: Text<'static>,
}

type CacheSlot = OnceLock<Mutex<Option<TextCache>>>;

static ART_CACHE: CacheSlot = OnceLock::new();
static SIDE_CACHE: CacheSlot = OnceLock::new();

fn cached_text(
    cache: &CacheSlot,
    key: (u8, u16, u16, u16),
    build: impl FnOnce() -> Text<'static>,
) -> Text<'static> {
    let slot = cache.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.as_ref().is_some_and(|cached| cached.key == key) {
        return guard.as_ref().unwrap().text.clone();
    }
    let text = build();
    *guard = Some(TextCache {
        key,
        text: text.clone(),
    });
    text
}

/// Splash principal (olho 96×36) para o transcript: amostra a grade para
/// `cols = min(96, width.saturating_sub(4))` e faz recorte central vertical
/// para `max_rows`. Cores truecolor; `Color::Indexed` (256) quando
/// `theme::detect_256()` indica terminal sem truecolor.
pub fn art_text(width: u16, max_rows: u16) -> Text<'static> {
    art_text_styled(width, max_rows, theme::detect_256())
}

/// Variante determinística (`indexed` fixa o modo de cor, imune ao `TERM` do
/// ambiente); `pub(crate)` para os testes da TUI.
pub(crate) fn art_text_styled(width: u16, max_rows: u16, indexed: bool) -> Text<'static> {
    let cols = u16::min(EYE_COLS as u16, width.saturating_sub(4)) as usize;
    let rows = u16::min(EYE_ROWS as u16, max_rows) as usize;
    let key = (0, u16::from(indexed), cols as u16, rows as u16);
    cached_text(&ART_CACHE, key, || {
        grid_text(eye_grid(), cols, rows, indexed)
    })
}

/// Olho compacto (26×12) para a sidebar; cabe em qualquer largura ≥ ~12.
/// Mesmo esquema de cores/queda do `art_text`.
pub fn side_text(width: u16, max_rows: u16) -> Text<'static> {
    side_text_styled(width, max_rows, theme::detect_256())
}

fn side_text_styled(width: u16, max_rows: u16, indexed: bool) -> Text<'static> {
    let cols = u16::min(SIDE_COLS as u16, width) as usize;
    let rows = u16::min(SIDE_ROWS as u16, max_rows) as usize;
    let key = (1, u16::from(indexed), cols as u16, rows as u16);
    cached_text(&SIDE_CACHE, key, || {
        grid_text(side_grid(), cols, rows, indexed)
    })
}

/// RGB → índice xterm 256: rampa de cinza (232..=255) quando r=g=b, senão o
/// cubo 6×6×6 (16..=231). Determinístico, sem dithering.
pub fn rgb_to_256((r, g, b): Rgb) -> u8 {
    if r == g && g == b {
        // Rampa de cinza: valores 8..=248 (passo 10) → 232..=255. Abaixo/da
        // rampa, o mais próximo é o canto do cubo (preto 16 / branco 231).
        if r < 4 {
            return 16; // mais perto do preto do cubo
        }
        if r > 246 {
            return 231; // mais perto do branco do cubo
        }
        return 232 + (u16::from(r).saturating_sub(8) / 10) as u8;
    }
    // Índice do eixo mais próximo (limiares 48/115, convenção xterm).
    let axis = |v: u8| -> u16 {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            (u16::from(v) - 35) / 40
        }
    };
    u8::try_from(16 + 36 * axis(r) + 6 * axis(g) + axis(b)).unwrap_or(231)
}

/// SGR combinado fg+bg em truecolor (`38;2` / `48;2`).
fn sgr((top, bottom): Cell) -> String {
    format!(
        "\x1b[38;2;{};{};{};48;2;{};{};{}m",
        top.0, top.1, top.2, bottom.0, bottom.1, bottom.2
    )
}

/// Side art em ANSI truecolor (linha por linha, SGR só quando fg/bg mudam).
fn side_ansi(cols: usize) -> String {
    let grid = side_grid();
    let rows = SIDE_ROWS;
    let top = crop_start(grid.rows, rows);
    let mut out = String::new();
    let mut current: Option<Cell> = None;
    for y in 0..rows {
        for x in 0..cols {
            let cell = grid.cell(top + y, sample_index(x, cols, grid.cols));
            if current != Some(cell) {
                out.push_str(&sgr(cell));
                current = Some(cell);
            }
            out.push('▀');
        }
        out.push_str("\x1b[0m\n");
        current = None; // reset no fim da linha força SGR na próxima
    }
    out
}

/// Splash para saída texto (REPL/headless via `println!`): ANSI truecolor
/// quando stdout é TTY; fora de TTY, uma linha simples (sem escapes).
pub fn splash_stdout(width: u16) -> String {
    use std::io::IsTerminal;
    let cols = SIDE_COLS.min(width as usize);
    if cols == 0 || !std::io::stdout().is_terminal() {
        return "zcode-cli".to_string();
    }
    side_ansi(cols)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture fixa: 3×2 com um token malformado no fim da 2ª linha.
    const FIXTURE: &str = "FMT1 3x2\n\
         ▀#FF0000/#00FF00 ▀#0000FF/#FFFFFF ▀#010203/#040506\n\
         ▀#FF47FE/#FF47FE ▀#FF47FE/#FF47FE ▀#zzzzzz/#FF47FE\n";

    #[test]
    fn parser_fmt1_fixture_conhecida() {
        let grid = parse_fmt1(FIXTURE);
        assert_eq!(grid.cols, 3);
        assert_eq!(grid.rows, 2);
        assert_eq!(grid.cells.len(), 5, "token malformado deve ser pulado");
        assert_eq!(grid.cells[0], ((0xFF, 0x00, 0x00), (0x00, 0xFF, 0x00)));
        assert_eq!(grid.cells[1], ((0x00, 0x00, 0xFF), (0xFF, 0xFF, 0xFF)));
        assert_eq!(grid.cells[2], ((0x01, 0x02, 0x03), (0x04, 0x05, 0x06)));
        assert_eq!(grid.cells[3], ((0xFF, 0x47, 0xFE), (0xFF, 0x47, 0xFE)));
        // Célula faltante (arquivo truncado) amostra como preto, sem panic.
        assert_eq!(grid.cell(1, 2), ((0, 0, 0), (0, 0, 0)));
        // Cabeçalho inválido → grade vazia (sem panic).
        let vazia = parse_fmt1("isto não é FMT1\n▀#000000/#000000");
        assert_eq!(vazia.cols, 0);
        assert_eq!(vazia.rows, 0);
        assert!(vazia.cells.is_empty());
    }

    #[test]
    fn parser_aceita_assets_embutidos() {
        let eye = parse_fmt1(EYE_ART);
        assert_eq!(eye.cols, 96);
        assert_eq!(eye.rows, 36);
        assert_eq!(eye.cells.len(), 96 * 36);
        let side = parse_fmt1(EYE_ART_SIDE);
        assert_eq!(side.cols, 26);
        assert_eq!(side.rows, 12);
        assert_eq!(side.cells.len(), 26 * 12);
    }

    #[test]
    fn rgb_to_256_casos_canonicos() {
        assert_eq!(rgb_to_256((0, 0, 0)), 16); // preto → canto do cubo
        assert_eq!(rgb_to_256((255, 255, 255)), 231); // branco → canto do cubo
        assert_eq!(rgb_to_256((255, 0, 0)), 196); // vermelho puro
        assert_eq!(rgb_to_256((128, 128, 128)), 244); // cinza médio (#808080)
        assert_eq!(rgb_to_256((0, 0, 255)), 21); // azul puro
        assert_eq!(rgb_to_256((5, 5, 5)), 232); // quase preto → rampa (#080808)
        assert_eq!(rgb_to_256((3, 3, 3)), 16); // bem embaixo da rampa → cubo
        assert_eq!(rgb_to_256((247, 247, 247)), 231); // acima da rampa → branco
        assert_eq!(rgb_to_256((238, 238, 238)), 255); // topo da rampa
    }

    #[test]
    fn art_text_dimensoes_por_largura() {
        // width 120 → cols = min(96, 116) = 96; rows completas.
        let wide = art_text_styled(120, 36, false);
        assert_eq!(wide.lines.len(), 36);
        assert!(wide
            .lines
            .iter()
            .all(|line| ratatui::text::Line::width(line) == 96));
        // width 60 → cols = 56.
        let mid = art_text_styled(60, 36, false);
        assert!(mid
            .lines
            .iter()
            .all(|line| ratatui::text::Line::width(line) == 56));
        // Recorte central vertical: max_rows 10 → 10 linhas.
        let cropped = art_text_styled(120, 10, false);
        assert_eq!(cropped.lines.len(), 10);
        // Largura mínima: sem célula → Text vazio (sem panic).
        assert!(art_text_styled(4, 10, false).lines.is_empty());
        // Determinismo: mesma chamada, mesmo texto.
        assert_eq!(
            art_text_styled(80, 16, false),
            art_text_styled(80, 16, false)
        );
    }

    #[test]
    fn side_text_dimensoes_e_fundo_magenta() {
        let art = side_text_styled(26, 12, false);
        assert_eq!(art.lines.len(), 12);
        assert!(art
            .lines
            .iter()
            .all(|line| ratatui::text::Line::width(line) == 26));
        // O fundo magenta dominante (#FF47FE) DEVE existir como bg de spans.
        let magentas = art
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.style.bg == Some(Color::Rgb(255, 71, 254)))
            .count();
        assert!(magentas > 0, "fundo magenta ausente da side art");
        // Todos os spans têm fg E bg definidos (half-block exige os dois).
        assert!(art.lines.iter().flat_map(|l| l.spans.iter()).all(|span| {
            matches!(span.style.fg, Some(Color::Rgb(..)))
                && matches!(span.style.bg, Some(Color::Rgb(..)))
        }));
        // Cabe em largura pequena: sampling reduz, sem panic.
        let estreita = side_text_styled(12, 6, false);
        assert_eq!(estreita.lines.len(), 6);
        assert!(estreita
            .lines
            .iter()
            .all(|line| ratatui::text::Line::width(line) == 12));
    }

    #[test]
    fn art_text_indexed_usa_cor_256() {
        let art = art_text_styled(40, 8, true);
        let all_indexed = art
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .all(|span| {
                matches!(span.style.fg, Some(Color::Indexed(_)))
                    && matches!(span.style.bg, Some(Color::Indexed(_)))
            });
        assert!(all_indexed, "modo 256 deve usar Color::Indexed");
        // (128,128,128) não existe na arte; o índice 244 (cinza) aparece?
        // Basta garantir que os índices caem no cubo/rampa válidos:
        assert!(art
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .all(|span| matches!(span.style.bg, Some(Color::Indexed(i)) if (16..=255).contains(&i))));
    }

    #[test]
    fn side_ansi_pula_sgr_em_repeticoes() {
        let full = side_ansi(26);
        // 12 linhas, cada uma termina com reset.
        assert_eq!(full.matches("\x1b[0m\n").count(), 12);
        // Truecolor 24-bit presente.
        assert!(full.contains("\x1b[38;2;255;71;254;48;2;"));
        // SGR só na troca de cor: menos resets de cor que células (26×12).
        let sgr_count = full.matches("\x1b[38;2;").count();
        assert!(sgr_count < 26 * 12, "SGR deve ser pulado em repetições");
        assert!(sgr_count > 12, "deve haver trocas de cor na arte");
        // Largura zero → fallback texto simples (determinístico, sem depender
        // do estado de TTY do stdout no ambiente de teste).
        assert_eq!(splash_stdout(0), "zcode-cli");
    }
}
