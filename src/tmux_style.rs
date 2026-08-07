//! Tmux style code parsing for ratatui rendering.

use ratatui::style::{Color, Modifier, Style};

/// Parse a string containing tmux style codes (`#[fg=#a6e3a1]`, `#[default]`, etc.)
/// into styled spans for ratatui rendering.
///
/// When the input contains no `#[` sequences, the entire string is returned as a
/// single span with `base_style`. Unclosed `#[` sequences are rendered literally.
pub fn parse_tmux_styles(input: &str, base_style: Style) -> Vec<(String, Style)> {
    let mut spans = Vec::new();
    let mut current_style = base_style;
    let mut remaining = input;

    while let Some(start) = remaining.find("#[") {
        // Emit text before the `#[`
        if start > 0 {
            spans.push((remaining[..start].to_string(), current_style));
        }
        remaining = &remaining[start + 2..];

        if let Some(end) = remaining.find(']') {
            let style_str = &remaining[..end];
            current_style = apply_tmux_directives(current_style, style_str, base_style);
            remaining = &remaining[end + 1..];
        } else {
            // Unclosed `#[` - render the rest literally
            spans.push(("#[".to_string(), current_style));
            break;
        }
    }

    if !remaining.is_empty() {
        spans.push((remaining.to_string(), current_style));
    }

    spans
}

/// Remove tmux style directives while preserving their visible text.
///
/// Backends without tmux-style rendering use this to avoid displaying raw
/// directives such as `#[fg=green]` in titles.
pub fn strip_tmux_styles(input: &str) -> String {
    parse_tmux_styles(input, Style::default())
        .into_iter()
        .map(|(text, _)| text)
        .collect()
}

/// Apply comma-separated tmux style directives (e.g. `fg=#a6e3a1,bold`) to a style.
pub fn apply_tmux_directives(mut current: Style, style_str: &str, base: Style) -> Style {
    for part in style_str.split(',') {
        let part = part.trim();
        if part.eq_ignore_ascii_case("default") || part.eq_ignore_ascii_case("none") {
            current = base;
        } else if let Some(fg) = part.strip_prefix("fg=") {
            if fg.eq_ignore_ascii_case("default") {
                current.fg = base.fg;
            } else if let Some(c) = parse_tmux_color(fg) {
                current = current.fg(c);
            }
        } else if let Some(bg) = part.strip_prefix("bg=") {
            if bg.eq_ignore_ascii_case("default") {
                current.bg = base.bg;
            } else if let Some(c) = parse_tmux_color(bg) {
                current = current.bg(c);
            }
        } else if part.eq_ignore_ascii_case("bold") {
            current = current.add_modifier(Modifier::BOLD);
        } else if part.eq_ignore_ascii_case("dim") {
            current = current.add_modifier(Modifier::DIM);
        } else if part.eq_ignore_ascii_case("italics") {
            current = current.add_modifier(Modifier::ITALIC);
        } else if part.eq_ignore_ascii_case("underscore") {
            current = current.add_modifier(Modifier::UNDERLINED);
        } else if part.eq_ignore_ascii_case("reverse") {
            current = current.add_modifier(Modifier::REVERSED);
        } else if part.eq_ignore_ascii_case("strikethrough") {
            current = current.add_modifier(Modifier::CROSSED_OUT);
        } else if part.eq_ignore_ascii_case("nobold") {
            current = current.remove_modifier(Modifier::BOLD);
        } else if part.eq_ignore_ascii_case("nodim") {
            current = current.remove_modifier(Modifier::DIM);
        } else if part.eq_ignore_ascii_case("noitalics") {
            current = current.remove_modifier(Modifier::ITALIC);
        } else if part.eq_ignore_ascii_case("nounderscore") {
            current = current.remove_modifier(Modifier::UNDERLINED);
        } else if part.eq_ignore_ascii_case("noreverse") {
            current = current.remove_modifier(Modifier::REVERSED);
        } else if part.eq_ignore_ascii_case("nostrikethrough") {
            current = current.remove_modifier(Modifier::CROSSED_OUT);
        }
        // Other unknown directives are silently ignored
    }
    current
}

/// Parse a tmux color value into a ratatui Color.
/// Supports `#RRGGBB` hex, `colour0`-`colour255`/`color0`-`color255`, and named colors.
fn parse_tmux_color(s: &str) -> Option<Color> {
    // Hex: #RRGGBB
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 && hex.is_ascii() {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }

    // colour/color index: colour0-colour255
    let idx_str = s.strip_prefix("colour").or_else(|| s.strip_prefix("color"));
    if let Some(idx_str) = idx_str
        && let Ok(idx) = idx_str.parse::<u8>()
    {
        return Some(Color::Indexed(idx));
    }

    // Named colors
    match s.to_ascii_lowercase().as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "white" => Some(Color::White),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_string_no_tmux_codes() {
        let base = Style::default().fg(Color::Cyan);
        let spans = parse_tmux_styles("🤖", base);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0, "🤖");
        assert_eq!(spans[0].1, base);
    }

    #[test]
    fn test_strip_tmux_styles_preserves_visible_text() {
        assert_eq!(
            strip_tmux_styles("#[fg=#a6e3a1]done#[default] now"),
            "done now"
        );
    }

    #[test]
    fn test_tmux_fg_hex_color() {
        let base = Style::default().fg(Color::Green);
        let spans = parse_tmux_styles("#[fg=#a6e3a1]\u{f0134} #[fg=default]", base);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0, "\u{f0134} ");
        assert_eq!(spans[0].1.fg, Some(Color::Rgb(0xa6, 0xe3, 0xa1)));
    }

    #[test]
    fn test_tmux_fg_hex_color_with_trailing_text() {
        let base = Style::default().fg(Color::Green);
        let spans = parse_tmux_styles("#[fg=#a6e3a1]\u{f0134}#[fg=default] done", base);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].0, "\u{f0134}");
        assert_eq!(spans[0].1.fg, Some(Color::Rgb(0xa6, 0xe3, 0xa1)));
        assert_eq!(spans[1].0, " done");
        assert_eq!(spans[1].1.fg, base.fg);
    }

    #[test]
    fn test_tmux_default_resets_to_base() {
        let base = Style::default().fg(Color::Magenta);
        let spans = parse_tmux_styles("#[fg=red]X#[default]Y", base);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].0, "X");
        assert_eq!(spans[0].1.fg, Some(Color::Red));
        assert_eq!(spans[1].0, "Y");
        assert_eq!(spans[1].1, base);
    }

    #[test]
    fn test_malformed_unclosed_bracket() {
        let base = Style::default().fg(Color::Cyan);
        let spans = parse_tmux_styles("icon #[fg=red", base);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].0, "icon ");
        assert_eq!(spans[1].0, "#[");
        assert_eq!(spans[2].0, "fg=red");
    }

    #[test]
    fn test_named_colors() {
        let base = Style::default();
        let spans = parse_tmux_styles("#[fg=red]R#[fg=blue]B", base);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].1.fg, Some(Color::Red));
        assert_eq!(spans[1].1.fg, Some(Color::Blue));
    }

    #[test]
    fn test_colour_index() {
        let base = Style::default();
        let spans = parse_tmux_styles("#[fg=colour196]X", base);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].1.fg, Some(Color::Indexed(196)));
    }

    #[test]
    fn test_bg_color() {
        let base = Style::default();
        let spans = parse_tmux_styles("#[bg=#ff0000]X", base);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].1.bg, Some(Color::Rgb(255, 0, 0)));
    }

    #[test]
    fn test_comma_separated_directives() {
        let base = Style::default();
        let spans = parse_tmux_styles("#[fg=red,bg=blue]X", base);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].1.fg, Some(Color::Red));
        assert_eq!(spans[0].1.bg, Some(Color::Blue));
    }

    #[test]
    fn test_bold_modifier() {
        let base = Style::default().fg(Color::Cyan);
        let spans = parse_tmux_styles("#[bold,fg=red]X", base);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].1.fg, Some(Color::Red));
        assert!(spans[0].1.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn test_unknown_directive_ignored() {
        let base = Style::default().fg(Color::Cyan);
        let spans = parse_tmux_styles("#[overline,fg=red]X", base);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].1.fg, Some(Color::Red));
    }

    #[test]
    fn test_nodim_negates_intrinsic_dim() {
        // The user style is built on top of `Style::default()`; a `nodim`
        // directive must record DIM in the sub_modifier so that
        // `base.patch(user)` strips DIM from the intrinsic style.
        let base = Style::default();
        let spans = parse_tmux_styles("#[fg=cyan,nodim]X", base);
        assert_eq!(spans.len(), 1);
        let user_style = spans[0].1;
        assert_eq!(user_style.fg, Some(Color::Cyan));
        assert!(user_style.sub_modifier.contains(Modifier::DIM));

        // Simulate the render path: an intrinsic style with DIM patched by
        // the user overlay should end up without DIM.
        let intrinsic = Style::default().add_modifier(Modifier::DIM);
        let patched = intrinsic.patch(user_style);
        assert!(!patched.add_modifier.contains(Modifier::DIM));
        assert_eq!(patched.fg, Some(Color::Cyan));
    }

    #[test]
    fn test_empty_input() {
        let base = Style::default();
        let spans = parse_tmux_styles("", base);
        assert!(spans.is_empty());
    }
}
