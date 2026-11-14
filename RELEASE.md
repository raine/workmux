# Releasing

Requires [rust-release-tools](https://github.com/raine/rust-release-tools):

```bash
pipx install git+https://github.com/raine/rust-release-tools.git
```

To release:

```bash
just release
```

This will:

1. Bump the version in Cargo.toml
2. Generate a changelog entry using Claude
3. Open an editor to review the changelog
4. Commit, tag, and push

GitHub Actions then builds the release binaries, creates the GitHub release,
publishes the crate to crates.io, and updates the Homebrew tap. Transient
crates.io and network failures are retried automatically.

Crates.io publishing requires the `CARGO_REGISTRY_TOKEN` GitHub Actions secret.

## Backfilling changelog

To generate changelog entries for all git tags missing from CHANGELOG.md:

```bash
update-changelog
```

This uses `cc-batch` to process multiple tags in parallel.
