# Repository Guidelines

## Project Structure & Module Organization
- This repo is a Rust workspace with service and library crates under directories such as `rsky-pds`, `rsky-relay`, `rsky-feedgen`, `rsky-common`, and related support crates.
- Shared automation and release helpers live in `scripts/`; existing CI workflows live in `.github/workflows/`.
- Review crate-local `Cargo.toml` files before making changes so edits stay scoped to the right workspace member.

## Build, Test, and Development Commands
- `cargo test --workspace`: run workspace tests.
- `cargo check --workspace`: validate builds across crates.
- `cargo fmt --check`: verify formatting.
- Use the existing repo scripts or workflows when crate-specific setup is needed.

## Coding Style & Naming Conventions
- Keep workspace changes narrowly scoped; avoid drive-by refactors across unrelated crates.
- Temporary or transitional code must include `TODO(#issue):` with a tracking issue.
- Follow existing Rust module and crate naming patterns.

## Pull Request Guardrails
- PR titles must use Conventional Commit format: `type(scope): summary` or `type: summary`.
- Set the correct PR title when opening the PR. Do not rely on fixing it later.
- If a PR title changes after opening, verify the semantic PR check reruns successfully.
- PR descriptions should include summary, motivation, linked issue when applicable, and manual test notes.
- Public PRs, issues, branch names, screenshots, and descriptions must not mention corporate partners, customers, brands, campaign names, or other sensitive external identities unless a maintainer explicitly approves it. Use generic descriptors instead.
