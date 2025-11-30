//! Prompt loading and parsing logic.
//!
//! This module extracts prompt-related domain logic from the command layer,
//! making it reusable and testable.

use crate::prompt::{Prompt, PromptDocument, PromptMetadata, parse_prompt_document};
use anyhow::{Context, Result, anyhow};
use edit::Builder;
use std::path::PathBuf;

/// Arguments for loading a prompt.
pub struct PromptLoadArgs<'a> {
    pub prompt_editor: bool,
    pub prompt_inline: Option<&'a str>,
    pub prompt_file: Option<&'a PathBuf>,
}

/// Load a prompt from the provided arguments (editor, inline, or file).
pub fn load_prompt(args: &PromptLoadArgs) -> Result<Option<Prompt>> {
    if args.prompt_editor {
        let mut builder = Builder::new();
        builder.suffix(".md");
        let editor_content = edit::edit_with_builder("", &builder)
            .context("Failed to open editor or read content")?;
        let trimmed = editor_content.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("Aborting: prompt is empty"));
        }
        Ok(Some(Prompt::Inline(trimmed.to_string())))
    } else {
        Ok(match (args.prompt_inline, args.prompt_file) {
            (Some(inline), None) => Some(Prompt::Inline(inline.to_string())),
            (None, Some(path)) => Some(Prompt::FromFile(path.clone())),
            (None, None) => None,
            _ => None, // clap enforces exclusivity; this is unreachable
        })
    }
}

/// Parse a prompt with optional frontmatter extraction.
///
/// Returns a PromptDocument with parsed metadata and body.
/// For inline prompts without editor, frontmatter is not parsed.
pub fn parse_prompt_with_frontmatter(
    prompt: &Prompt,
    from_editor_or_file: bool,
) -> Result<PromptDocument> {
    if from_editor_or_file {
        parse_prompt_document(prompt)
    } else {
        // Inline prompt without editor: no frontmatter parsing
        Ok(PromptDocument {
            body: match prompt {
                Prompt::Inline(s) => s.clone(),
                Prompt::FromFile(_) => unreachable!(),
            },
            meta: PromptMetadata::default(),
        })
    }
}
