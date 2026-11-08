# AGENTS.md

Instructions for AI coding agents working in this repository. Applies to the entire repo.

## Language policy

English is the default and primary language of this repository.

- All code comments must be written in English, across Rust, TypeScript, JavaScript, SQL, shell, and config files.
- Identifiers, log messages, error messages, commit messages, and PR titles/descriptions are written in English.
- `README.md` is English and is the default entry point. `docs/` content may be translated, but English comes first: an English version must exist and is the source of truth. Translations live in separate files (e.g. `README.zh-CN.md`) and must not replace or preempt the English version.
- When Chinese and English versions conflict, the English version wins.

## Living docs

- Tracked engineering decisions live in `.livingdocs/` as an OKF bundle: the constitution, the v0.1 PRD, ADRs, reference-model notes, and living Mermaid architecture views.
- Read the in-force view with `living-docs read --docs-dir .livingdocs`; do not read `index.md` directly.
- Before changing a decision, read the relevant record and use `living-docs supersede`; never rewrite an accepted record in place.
- After a structural change, run `living-docs check .livingdocs` and update the affected ADR or architecture view in the same change.
- `docs/` remains local-only scratch space and is not part of the living-docs bundle.

## `docs/` is local-only

- `docs/` holds local design notes and drafts. It is git-ignored and must never be committed.
- Never run `git add` on anything under `docs/`. Do not create tracked files there.
- Do not treat `docs/` as a source of truth for implementation; it is scratch context only.

## Working agreement

- Do not create commits unless the user explicitly asks.
- Do not add documentation files beyond what the task requires.
