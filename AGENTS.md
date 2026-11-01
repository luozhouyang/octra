# AGENTS.md

Instructions for AI coding agents working in this repository. Applies to the entire repo.

## Language policy

English is the default and primary language of this repository.

- All code comments must be written in English, across Rust, TypeScript, JavaScript, SQL, shell, and config files.
- Identifiers, log messages, error messages, commit messages, and PR titles/descriptions are written in English.
- `README.md` is English and is the default entry point. `docs/` content may be translated, but English comes first: an English version must exist and is the source of truth. Translations live in separate files (e.g. `README.zh-CN.md`) and must not replace or preempt the English version.
- When Chinese and English versions conflict, the English version wins.

## `docs/` is local-only

- `docs/` holds local design notes and drafts. It is git-ignored and must never be committed.
- Never run `git add` on anything under `docs/`. Do not create tracked files there.
- Do not treat `docs/` as a source of truth for implementation; it is scratch context only.

## Working agreement

- Do not create commits unless the user explicitly asks.
- Do not add documentation files beyond what the task requires.
