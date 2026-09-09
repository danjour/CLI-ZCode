//! Visual themes for the ratatui shell.
//!
//! The terminal surfaces are pure black, with purple, blue, magenta, cyan and
//! gold foreground accents. As cores da splash NÃO moram aqui: a splash é a
//! arte half-block truecolor de `ui::art`, que traz as próprias cores por
//! célula (fg+bg). Este módulo mantém as paletas do resto da UI.

use ratatui::{style::Color, widgets::BorderType};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum Theme {
    #[default]
    Dark,
    Light,
    /// Green phosphor opt-in (`ui.theme = "retro"`).
    Retro,
}

impl Theme {
    /// Reads the configured theme. Unknown or missing values stay Dark.
    pub fn from_name(name: &str) -> Self {
        match name.trim().to_lowercase().as_str() {
            "light" | "claro" => Self::Light,
            "retro" | "fosforo" | "phosphor" => Self::Retro,
            _ => Self::Dark,
        }
    }

    pub fn palette(&self) -> Palette {
        match self {
            Self::Dark => Palette {
                background: Color::Black,
                surface: Color::Black,
                user: Color::Rgb(94, 218, 255),
                assistant: Color::Rgb(237, 239, 255),
                system: Color::Rgb(255, 207, 112),
                border: Color::Rgb(126, 91, 173),
                prompt: Color::Rgb(255, 97, 203),
                muted: Color::Rgb(158, 143, 190),
                badge_fg: Color::Rgb(123, 232, 255),
                status_fg: Color::White,
                status_bg: Color::Black,
                // Cinza escuro VISÍVEL contra o fundo preto (fg do código
                // herda a cor do papel, clara — segue legível sobre ele).
                code_bg: Color::Rgb(36, 36, 48),
                sep: Color::Rgb(70, 47, 103),
                frame: BorderType::Plain,
            },
            Self::Light => Palette {
                background: Color::Black,
                surface: Color::Black,
                user: Color::LightCyan,
                assistant: Color::White,
                system: Color::LightYellow,
                border: Color::LightBlue,
                prompt: Color::LightMagenta,
                muted: Color::Gray,
                badge_fg: Color::LightCyan,
                status_fg: Color::White,
                status_bg: Color::Black,
                // "Light" muda só os fg (fundo continua preto): cinza escuro
                // igual ao Dark mantém o fg claro do código legível.
                code_bg: Color::Rgb(36, 36, 48),
                sep: Color::Gray,
                frame: BorderType::Rounded,
            },
            Self::Retro => Palette {
                background: Color::Black,
                surface: Color::Black,
                user: Color::Green,
                assistant: Color::LightGreen,
                system: Color::LightGreen,
                border: Color::Green,
                prompt: Color::LightGreen,
                muted: Color::Green,
                badge_fg: Color::LightGreen,
                status_fg: Color::LightGreen,
                status_bg: Color::Black,
                code_bg: Color::Rgb(36, 36, 48),
                sep: Color::DarkGray,
                frame: BorderType::Double,
            },
        }
    }

    /// 256-color counterpart of `palette`, used when TERM advertises it.
    pub fn palette_256(&self) -> Palette {
        match self {
            Self::Dark => Palette {
                background: Color::Black,
                surface: Color::Black,
                user: Color::Indexed(110),
                assistant: Color::Indexed(255),
                system: Color::Indexed(222),
                border: Color::Indexed(240),
                prompt: Color::Indexed(205),
                muted: Color::Indexed(146),
                badge_fg: Color::Indexed(123),
                status_fg: Color::Indexed(255),
                status_bg: Color::Black,
                // Equivalente 256 do Rgb(36, 36, 48): cinza escuro visível.
                code_bg: Color::Indexed(236),
                sep: Color::Indexed(239),
                frame: BorderType::Plain,
            },
            Self::Light => Palette {
                background: Color::Black,
                surface: Color::Black,
                user: Color::Indexed(159),
                assistant: Color::Indexed(255),
                system: Color::Indexed(229),
                border: Color::Indexed(244),
                prompt: Color::Indexed(213),
                muted: Color::Indexed(250),
                badge_fg: Color::Indexed(159),
                status_fg: Color::Indexed(255),
                status_bg: Color::Black,
                // "Light" muda só os fg (fundo continua preto): igual ao Dark.
                code_bg: Color::Indexed(236),
                sep: Color::Indexed(250),
                frame: BorderType::Rounded,
            },
            Self::Retro => Palette {
                background: Color::Black,
                surface: Color::Black,
                user: Color::Indexed(46),
                assistant: Color::Indexed(120),
                system: Color::Indexed(120),
                border: Color::Indexed(40),
                prompt: Color::Indexed(120),
                muted: Color::Indexed(46),
                badge_fg: Color::Indexed(120),
                status_fg: Color::Indexed(120),
                status_bg: Color::Black,
                code_bg: Color::Indexed(236),
                sep: Color::Indexed(22),
                frame: BorderType::Double,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub background: Color,
    pub surface: Color,
    pub user: Color,
    pub assistant: Color,
    pub system: Color,
    pub border: Color,
    pub prompt: Color,
    pub muted: Color,
    pub badge_fg: Color,
    pub status_fg: Color,
    pub status_bg: Color,
    /// Fundo dos blocos de código (inline/fence): PRECISA contrastar com
    /// `background` (preto em todas as paletas) — cinza escuro visível, pois
    /// o fg do código herda a cor do papel (clara em todos os temas,
    /// inclusive Light, que muda só os fg).
    pub code_bg: Color,
    pub sep: Color,
    pub frame: BorderType,
}

/// Decisão pura de paleta: `true` = paleta 256, `false` = truecolor/nativo.
/// 256 só quando o TERM anuncia `256color` E o COLORTERM NÃO anuncia
/// truecolor (`truecolor`/`24bit` valem mesmo com TERM=screen-256color etc.).
fn decide_palette(term: Option<&str>, colorterm: Option<&str>) -> bool {
    let truecolor = colorterm.is_some_and(|c| {
        let c = c.to_lowercase();
        c.contains("truecolor") || c.contains("24bit")
    });
    if truecolor {
        return false;
    }
    term.map(|t| t.contains("256color")).unwrap_or(false)
}

/// 256 colors are used only when TERM explicitly advertises them (and
/// COLORTERM does not advertise truecolor).
pub fn detect_256() -> bool {
    decide_palette(
        std::env::var("TERM").ok().as_deref(),
        std::env::var("COLORTERM").ok().as_deref(),
    )
}

/// Shell prompt shown before the first input line.
pub fn prompt() -> &'static str {
    "$ "
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_palette_truecolor_vence_256() {
        // Sem TERM/COLORTERM (Windows típico): truecolor.
        assert!(!decide_palette(None, None));
        // COLORTERM truecolor/24bit vence mesmo com TERM anunciando 256.
        assert!(!decide_palette(Some("screen-256color"), Some("truecolor")));
        assert!(!decide_palette(Some("xterm-256color"), Some("TrueColor")));
        assert!(!decide_palette(Some("xterm-256color"), Some("24bit")));
        // TERM anuncia 256 sem COLORTERM: paleta 256.
        assert!(decide_palette(Some("xterm-256color"), None));
        assert!(decide_palette(Some("xterm-256color"), Some("")));
        // TERM sem 256color: truecolor (mantém o comportamento atual).
        assert!(!decide_palette(Some("dumb"), None));
        assert!(!decide_palette(Some("xterm"), None));
    }

    #[test]
    fn tema_do_config() {
        assert_eq!(Theme::from_name("light"), Theme::Light);
        assert_eq!(Theme::from_name("dark"), Theme::Dark);
        assert_eq!(Theme::from_name("qualquer"), Theme::Dark);
        assert_eq!(Theme::default(), Theme::Dark);
    }

    #[test]
    fn paletas_diferem() {
        assert_ne!(Theme::Dark.palette(), Theme::Light.palette());
        for theme in [Theme::Dark, Theme::Light, Theme::Retro] {
            for palette in [theme.palette(), theme.palette_256()] {
                assert_eq!(palette.background, Color::Black);
                assert_eq!(palette.surface, Color::Black);
                assert_eq!(palette.status_bg, Color::Black);
                // Contraste real do código: code_bg ≠ fundo/superfície (antes
                // era Black igual ao fundo → bloco de código invisível).
                assert_ne!(palette.code_bg, palette.background);
                assert_ne!(palette.code_bg, palette.surface);
            }
        }
        // Valores exatos: truecolor cinza escuro; 256 usa o índice equivalente.
        for theme in [Theme::Dark, Theme::Light, Theme::Retro] {
            assert_eq!(theme.palette().code_bg, Color::Rgb(36, 36, 48));
            assert_eq!(theme.palette_256().code_bg, Color::Indexed(236));
        }
    }

    #[test]
    fn paleta_256_estendida() {
        assert_ne!(Theme::Dark.palette(), Theme::Dark.palette_256());
        assert_ne!(Theme::Light.palette(), Theme::Light.palette_256());
        assert_ne!(Theme::Dark.palette_256(), Theme::Light.palette_256());
    }

    #[test]
    fn retro_opt_in_default_intacto() {
        assert_eq!(Theme::from_name("retro"), Theme::Retro);
        assert_eq!(Theme::from_name("fosforo"), Theme::Retro);
        assert_eq!(Theme::from_name("phosphor"), Theme::Retro);
        assert_eq!(Theme::from_name("RETRO"), Theme::Retro);
        assert_eq!(Theme::default(), Theme::Dark);
        assert_ne!(Theme::Retro.palette(), Theme::Dark.palette());
        assert_ne!(Theme::Retro.palette(), Theme::Light.palette());
        assert_ne!(Theme::Retro.palette_256(), Theme::Dark.palette_256());
        assert_eq!(Theme::Retro.palette().frame, BorderType::Double);
        assert_eq!(Theme::Dark.palette().frame, BorderType::Plain);
    }

    #[test]
    fn retro_sem_tons_quentes() {
        fn warm(c: &Color) -> bool {
            matches!(
                c,
                Color::Yellow
                    | Color::LightYellow
                    | Color::Red
                    | Color::LightRed
                    | Color::Magenta
                    | Color::LightMagenta
            )
        }
        for palette in [Theme::Retro.palette(), Theme::Retro.palette_256()] {
            let all = [
                palette.background,
                palette.surface,
                palette.user,
                palette.assistant,
                palette.system,
                palette.border,
                palette.prompt,
                palette.muted,
                palette.badge_fg,
                palette.status_fg,
                palette.status_bg,
                palette.code_bg,
                palette.sep,
            ];
            for color in all {
                assert!(!warm(&color), "warm color in retro: {color:?}");
            }
        }
    }
}
