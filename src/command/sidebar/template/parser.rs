//! Template parser: converts a format string into a sequence of tokens.

use std::fmt;

/// A single token in a parsed template line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// Literal text (including escaped braces).
    Literal(String),
    /// A named field token.
    Field(TokenId),
    /// Layout fill marker.
    Fill,
    /// Tmux-style directive (`#[fg=red,bold]`). Carries the raw directive
    /// content (without the surrounding `#[` and `]`).
    Style(String),
}

/// Identifiers for every supported template token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenId {
    StatusIcon,
    AgentIcon,
    AgentLabel,
    Primary,
    Secondary,
    Worktree,
    Project,
    Session,
    Window,
    WindowIndex,
    PaneTitle,
    PaneSuffix,
    Elapsed,
    GitStats,
    GitCommitted,
    GitUncommitted,
    GitRebase,
    GitAhead,
    GitBehind,
    GitDirty,
    GitConflict,
    GitBranch,
    PrNumber,
    PrChecks,
    StatusLabel,
    Idx,
    JumpKey,
}

impl TokenId {
    /// Whether this token can absorb slack width on its line.
    pub fn is_flex(self) -> bool {
        matches!(
            self,
            TokenId::Primary
                | TokenId::Secondary
                | TokenId::Worktree
                | TokenId::Project
                | TokenId::Session
                | TokenId::Window
                | TokenId::PaneTitle
        )
    }
}

impl fmt::Display for TokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            TokenId::StatusIcon => "status_icon",
            TokenId::AgentIcon => "agent_icon",
            TokenId::AgentLabel => "agent_label",
            TokenId::Primary => "primary",
            TokenId::Secondary => "secondary",
            TokenId::Worktree => "worktree",
            TokenId::Project => "project",
            TokenId::Session => "session",
            TokenId::Window => "window",
            TokenId::WindowIndex => "window_index",
            TokenId::PaneTitle => "pane_title",
            TokenId::PaneSuffix => "pane_suffix",
            TokenId::Elapsed => "elapsed",
            TokenId::GitStats => "git_stats",
            TokenId::GitCommitted => "git_committed",
            TokenId::GitUncommitted => "git_uncommitted",
            TokenId::GitRebase => "git_rebase",
            TokenId::GitAhead => "git_ahead",
            TokenId::GitBehind => "git_behind",
            TokenId::GitDirty => "git_dirty",
            TokenId::GitConflict => "git_conflict",
            TokenId::GitBranch => "git_branch",
            TokenId::PrNumber => "pr_number",
            TokenId::PrChecks => "pr_checks",
            TokenId::StatusLabel => "status_label",
            TokenId::Idx => "idx",
            TokenId::JumpKey => "jump_key",
        };
        write!(f, "{}", s)
    }
}

/// Parse error with a human-friendly message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ParseError {}

/// Parse a single template line into tokens.
///
/// Supports:
/// - `{{` → literal `{`
/// - `}}` → literal `}`
/// - `{name}` → field token
/// - `{fill}` → fill marker
/// - `#[fg=red,bold]` → tmux-style directive token (zero-width). Unclosed
///   `#[...` falls back to literal text.
pub fn parse_line(input: &str) -> Result<Vec<Token>, ParseError> {
    let mut tokens = Vec::new();
    let mut chars = input.char_indices().peekable();
    let mut literal = String::new();
    let mut fill_count = 0;
    let mut saw_unclosed_style = false;

    while let Some((i, c)) = chars.next() {
        if c == '#'
            && let Some(&(_, '[')) = chars.peek()
        {
            // Consume the `[`
            chars.next();

            // Look for closing `]`. The directive body cannot contain `]`,
            // matching `tmux_style::parse_tmux_styles`.
            let mut directive = String::new();
            let mut found_close = false;
            for (_, inner_c) in chars.by_ref() {
                if inner_c == ']' {
                    found_close = true;
                    break;
                }
                directive.push(inner_c);
            }

            if found_close {
                // Flush pending literal, then emit the style token.
                if !literal.is_empty() {
                    tokens.push(Token::Literal(std::mem::take(&mut literal)));
                }
                tokens.push(Token::Style(directive));
            } else {
                // Unclosed `#[`: render the prefix and the consumed body
                // literally (matches `tmux_style::parse_tmux_styles`).
                literal.push_str("#[");
                literal.push_str(&directive);
                saw_unclosed_style = true;
            }
            continue;
        }

        if c == '{' {
            if let Some(&(_, next_c)) = chars.peek()
                && next_c == '{'
            {
                // Escaped brace
                literal.push('{');
                chars.next();
                continue;
            }

            // Look for closing brace
            let start = i;
            let mut name = String::new();
            let mut found_close = false;

            for (_, inner_c) in chars.by_ref() {
                if inner_c == '}' {
                    found_close = true;
                    break;
                }
                name.push(inner_c);
            }

            if !found_close {
                return Err(ParseError {
                    message: format!(
                        "unclosed brace at column {}: '{}'",
                        start + 1,
                        &input[start..]
                    ),
                });
            }

            if name.is_empty() {
                return Err(ParseError {
                    message: format!("empty token at column {}", start + 1),
                });
            }

            // Flush pending literal
            if !literal.is_empty() {
                tokens.push(Token::Literal(std::mem::take(&mut literal)));
            }

            if name == "fill" {
                tokens.push(Token::Fill);
                fill_count += 1;
            } else {
                let token_id = match name.as_str() {
                    "status_icon" => TokenId::StatusIcon,
                    "agent_icon" => TokenId::AgentIcon,
                    "agent_label" => TokenId::AgentLabel,
                    "primary" => TokenId::Primary,
                    "secondary" => TokenId::Secondary,
                    "worktree" => TokenId::Worktree,
                    "project" => TokenId::Project,
                    "session" => TokenId::Session,
                    "window" => TokenId::Window,
                    "window_index" => TokenId::WindowIndex,
                    "pane_title" => TokenId::PaneTitle,
                    "pane_suffix" => TokenId::PaneSuffix,
                    "elapsed" => TokenId::Elapsed,
                    "git_stats" => TokenId::GitStats,
                    "git_committed" => TokenId::GitCommitted,
                    "git_uncommitted" => TokenId::GitUncommitted,
                    "git_rebase" => TokenId::GitRebase,
                    "git_ahead" => TokenId::GitAhead,
                    "git_behind" => TokenId::GitBehind,
                    "git_dirty" => TokenId::GitDirty,
                    "git_conflict" => TokenId::GitConflict,
                    "git_branch" => TokenId::GitBranch,
                    "pr_number" => TokenId::PrNumber,
                    "pr_checks" => TokenId::PrChecks,
                    "status_label" => TokenId::StatusLabel,
                    "idx" => TokenId::Idx,
                    "jump_key" => TokenId::JumpKey,
                    other => {
                        return Err(ParseError {
                            message: format!("unknown token '{}' at column {}", other, start + 1),
                        });
                    }
                };
                tokens.push(Token::Field(token_id));
            }
        } else if c == '}' {
            if let Some(&(_, next_c)) = chars.peek()
                && next_c == '}'
            {
                literal.push('}');
                chars.next();
                continue;
            }
            // Lone closing brace - treat as literal
            literal.push('}');
        } else {
            literal.push(c);
        }
    }

    // Flush trailing literal
    if !literal.is_empty() {
        tokens.push(Token::Literal(literal));
    }

    if fill_count > 1 {
        return Err(ParseError {
            message: format!(
                "at most one {{fill}} allowed per line, found {}",
                fill_count
            ),
        });
    }

    if saw_unclosed_style {
        tracing::debug!(
            template = input,
            "sidebar template contained an unclosed `#[` style directive; rendering it as literal text"
        );
    }

    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_literal_only() {
        let tokens = parse_line("hello world").unwrap();
        assert_eq!(tokens, vec![Token::Literal("hello world".to_string())]);
    }

    #[test]
    fn parse_single_token() {
        let tokens = parse_line("{primary}").unwrap();
        assert_eq!(tokens, vec![Token::Field(TokenId::Primary)]);
    }

    #[test]
    fn parse_mixed_tokens() {
        let tokens = parse_line("{status_icon} {primary}{pane_suffix} {fill} {elapsed}").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Field(TokenId::StatusIcon),
                Token::Literal(" ".to_string()),
                Token::Field(TokenId::Primary),
                Token::Field(TokenId::PaneSuffix),
                Token::Literal(" ".to_string()),
                Token::Fill,
                Token::Literal(" ".to_string()),
                Token::Field(TokenId::Elapsed),
            ]
        );
    }

    #[test]
    fn parse_escaped_braces() {
        let tokens = parse_line("{{literal}}").unwrap();
        assert_eq!(tokens, vec![Token::Literal("{literal}".to_string())]);
    }

    #[test]
    fn parse_fill_token() {
        let tokens = parse_line("{primary} {fill} {elapsed}").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Field(TokenId::Primary),
                Token::Literal(" ".to_string()),
                Token::Fill,
                Token::Literal(" ".to_string()),
                Token::Field(TokenId::Elapsed),
            ]
        );
    }

    #[test]
    fn reject_unknown_token() {
        let err = parse_line("{unknown}").unwrap_err();
        assert!(err.message.contains("unknown token 'unknown'"));
    }

    #[test]
    fn reject_unclosed_brace() {
        let err = parse_line("{primary").unwrap_err();
        assert!(err.message.contains("unclosed brace"));
    }

    #[test]
    fn reject_empty_token() {
        let err = parse_line("{}").unwrap_err();
        assert!(err.message.contains("empty token"));
    }

    #[test]
    fn reject_multiple_fill() {
        let err = parse_line("{fill} {fill}").unwrap_err();
        assert!(err.message.contains("at most one {fill}"));
    }

    #[test]
    fn parse_style_directive() {
        let tokens = parse_line("#[fg=red]X").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Style("fg=red".to_string()),
                Token::Literal("X".to_string()),
            ]
        );
    }

    #[test]
    fn parse_multiple_styles_split_literals() {
        let tokens = parse_line("a#[fg=red]b#[default]c").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Literal("a".to_string()),
                Token::Style("fg=red".to_string()),
                Token::Literal("b".to_string()),
                Token::Style("default".to_string()),
                Token::Literal("c".to_string()),
            ]
        );
    }

    #[test]
    fn parse_style_around_token() {
        let tokens = parse_line("#[fg=cyan]{primary}#[default]").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Style("fg=cyan".to_string()),
                Token::Field(TokenId::Primary),
                Token::Style("default".to_string()),
            ]
        );
    }

    #[test]
    fn parse_unclosed_style_falls_back_to_literal() {
        let tokens = parse_line("icon #[fg=red").unwrap();
        assert_eq!(tokens, vec![Token::Literal("icon #[fg=red".to_string())]);
    }

    #[test]
    fn parse_empty_style_directive() {
        let tokens = parse_line("#[]X").unwrap();
        assert_eq!(
            tokens,
            vec![Token::Style(String::new()), Token::Literal("X".to_string()),]
        );
    }

    #[test]
    fn parse_lone_hash_treated_as_literal() {
        let tokens = parse_line("a#b#c").unwrap();
        assert_eq!(tokens, vec![Token::Literal("a#b#c".to_string())]);
    }

    #[test]
    fn all_token_ids_roundtrip() {
        for token_id in [
            TokenId::StatusIcon,
            TokenId::AgentIcon,
            TokenId::AgentLabel,
            TokenId::Primary,
            TokenId::Secondary,
            TokenId::Worktree,
            TokenId::Project,
            TokenId::Session,
            TokenId::Window,
            TokenId::WindowIndex,
            TokenId::PaneTitle,
            TokenId::PaneSuffix,
            TokenId::Elapsed,
            TokenId::GitStats,
            TokenId::GitCommitted,
            TokenId::GitUncommitted,
            TokenId::GitRebase,
            TokenId::GitAhead,
            TokenId::GitBehind,
            TokenId::GitDirty,
            TokenId::GitConflict,
            TokenId::GitBranch,
            TokenId::PrNumber,
            TokenId::PrChecks,
            TokenId::StatusLabel,
            TokenId::Idx,
            TokenId::JumpKey,
        ] {
            let name = token_id.to_string();
            let parsed = parse_line(&format!("{{{}}}", name)).unwrap();
            assert_eq!(parsed, vec![Token::Field(token_id)], "failed for {}", name);
        }
    }
}
