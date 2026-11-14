use anyhow::Result;

const CHANGELOG: &str = include_str!("../../CHANGELOG.md");

fn standalone_changelog(source: &str) -> String {
    let Some((_, body)) = source
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
    else {
        return source.to_owned();
    };

    format!("# Changelog\n\n{}", body.trim_start())
}

pub fn run() -> Result<()> {
    let changelog = standalone_changelog(CHANGELOG);
    crate::markdown::display(&changelog, &changelog);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::standalone_changelog;

    #[test]
    fn reconstructs_standalone_title_after_frontmatter() {
        let source = "---\ntitle: Changelog\ndescription: Releases\n---\n\n## v1.0.0\n";

        assert_eq!(standalone_changelog(source), "# Changelog\n\n## v1.0.0\n");
    }
}
