# Domain docs

This repo uses a single-context domain-doc layout.

## Before exploring

Read these files when they exist:

- `CONTEXT.md` at the repo root
- Relevant ADRs under `docs/adr/`

If they do not exist, proceed without flagging their absence. The `/domain-modeling` skill creates them when the project resolves terms or architectural decisions.

## File structure

```text
/
├── CONTEXT.md
├── docs/adr/
│   ├── 0001-example-decision.md
│   └── 0002-another-decision.md
└── src/
```

## Use the glossary's vocabulary

When output names a domain concept, use the term defined in `CONTEXT.md`. Do not replace it with a synonym the glossary rejects.

If a needed concept is missing, reconsider whether the term belongs or note the gap for `/domain-modeling`.

## Flag ADR conflicts

If proposed work contradicts an ADR, state the conflict instead of silently overriding it:

> Contradicts ADR-0007, but worth reopening because...
