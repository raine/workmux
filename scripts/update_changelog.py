#!/usr/bin/env python3
"""
Find git tags missing from CHANGELOG.md and use cc-batch to add them.
"""

import subprocess
import re
import tempfile
from pathlib import Path

PROMPT = """\
# Changelog Updater

## Task

Generate a user-focused changelog entry for a git tag and insert it into CHANGELOG.md.

## Input

- Git tag: `{input}`

## Steps

1. **Find the previous tag**

   ```bash
   git describe --tags --abbrev=0 '{input}^'
   ```

   If this fails (no previous tag exists), this is the first release—compare against the initial commit instead, or skip if there's nothing meaningful to document.

2. **Get the commits between tags**

   ```bash
   git log --oneline PREVIOUS_TAG..{input}
   ```

3. **Review the actual changes** to understand context

   ```bash
   git diff PREVIOUS_TAG..{input} --stat
   ```

   Read relevant changed files as needed to understand what the changes do.

4. **Consult Gemini for changelog summary**

   Use `mcp__consult-llm__consult_llm` to get help writing user-focused changelog entries:

   - `model`: "gemini-3-pro-preview"
   - `prompt`: Ask Gemini to summarize the changes for a changelog, focusing on user-facing impact. Include the commit log and diff stat in the prompt.
   - `files`: Include the key changed files as context

   Use Gemini's response to inform your changelog entry.

5. **Get the tag date**

   ```bash
   git log -1 --format=%as {input}
   ```

6. **Update CHANGELOG.md**

   - If CHANGELOG.md doesn't exist, create it with `# Changelog` header
   - Insert the new entry in the correct position by version order (newest first)
   - Use heading level `##` for version entries

## Writing Guidelines

- Write from the user's perspective: what can they now do, what's fixed, what's improved?
- Use plain language, avoid implementation details like function names, file paths, or internal refactors
- Focus on behavior changes, new capabilities, and bug fixes users would notice
- Skip purely internal changes (refactoring, test improvements, CI changes) unless they affect users
- Keep entries concise—one line per change is usually enough
- If a tag has no user-facing changes, skip it entirely—don't modify CHANGELOG.md

## Entry Format

```markdown
## {input} (YYYY-MM-DD)

- Change description
- Another change description
```
"""


def get_all_tags() -> list[str]:
    """Get all git tags sorted by version (newest first)."""
    result = subprocess.run(
        ["git", "tag", "--sort=-version:refname"],
        capture_output=True,
        text=True,
        check=True,
    )
    return [t.strip() for t in result.stdout.strip().split("\n") if t.strip()]


def get_tags_in_changelog() -> set[str]:
    """Get tags already present in CHANGELOG.md (entries or skipped markers)."""
    try:
        with open("CHANGELOG.md") as f:
            content = f.read()
        tags = set()
        # Match ## v0.1.5 or # v0.1.5 style headings (with optional date)
        heading_pattern = r"^#+\s+(v[\d.]+)"
        tags.update(re.findall(heading_pattern, content, re.MULTILINE))
        # Match <!-- skipped: v0.1.5 --> comments
        skipped_pattern = r"<!--\s*skipped:\s*(v[\d.]+)\s*-->"
        tags.update(re.findall(skipped_pattern, content))
        return tags
    except FileNotFoundError:
        return set()


def insert_skipped_comments(tags: list[str]) -> None:
    """Insert <!-- skipped: vX.X.X --> comments for tags that weren't added."""
    if not tags:
        return
    try:
        with open("CHANGELOG.md") as f:
            content = f.read()
    except FileNotFoundError:
        return

    # Insert skipped comments at the top, after the # Changelog header
    skipped_block = "\n".join(f"<!-- skipped: {tag} -->" for tag in tags)
    # Find the end of the first line (the header)
    header_end = content.find("\n")
    if header_end == -1:
        content = content + "\n\n" + skipped_block
    else:
        content = content[: header_end + 1] + "\n" + skipped_block + content[header_end + 1 :]

    with open("CHANGELOG.md", "w") as f:
        f.write(content)


def main():
    all_tags = get_all_tags()
    existing_tags = get_tags_in_changelog()

    missing_tags = [t for t in all_tags if t not in existing_tags]

    if not missing_tags:
        print("All tags are already in CHANGELOG.md")
        return

    print(f"Found {len(missing_tags)} missing tags: {', '.join(missing_tags)}")

    # Write prompt to temp file and run cc-batch
    with tempfile.NamedTemporaryFile(
        mode="w", suffix=".md", delete=False
    ) as prompt_file:
        prompt_file.write(PROMPT)
        prompt_path = prompt_file.name

    try:
        tags_input = "\n".join(missing_tags)
        subprocess.run(
            ["cc-batch", prompt_path, "--dangerously-skip-permissions"],
            input=tags_input,
            text=True,
        )
    finally:
        Path(prompt_path).unlink(missing_ok=True)

    # Check which tags are still missing and mark them as skipped
    updated_tags = get_tags_in_changelog()
    still_missing = [t for t in missing_tags if t not in updated_tags]
    if still_missing:
        print(f"Marking {len(still_missing)} tags as skipped: {', '.join(still_missing)}")
        insert_skipped_comments(still_missing)

    # Format with prettier
    subprocess.run(["prettier", "--write", "CHANGELOG.md"], capture_output=True)


if __name__ == "__main__":
    main()
