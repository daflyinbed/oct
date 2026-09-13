# ADR 0001: Provider/Model/Project Schema Redesign

**Status**: Accepted
**Date**: 2026-04-18

## Context

The original schema had a flat `conversations` table with `provider_spec` and `working_dir` columns, and a global `AppState.provider_spec` configured via environment variable. This design had several limitations:

1. No project concept — conversations were not grouped
2. No provider/model management — model selection was global only
3. No way to configure multiple providers or custom OpenAI-compatible endpoints at runtime
4. No visibility into available models and their capabilities

We want to:

- Support a global provider/model catalog (read-only builtin data from models.dev schema, plus user-added custom providers)
- Introduce a Project entity that groups conversations under a shared working directory
- Allow per-message model selection instead of per-project/per-conversation

## Decision

### Database Schema

Five core tables:

```
providers  →  models  (1:N)
projects   →  conversations (1:N)
conversations → messages (1:N)
providers/models → messages (denormalized reference)
```

**`providers`** — Provider catalog (Anthropic, OpenAI, custom OpenAI-compatible). `source` column distinguishes `builtin` (read-only, seeded from models.dev data) from `custom` (user-managed).

**`models`** — Model catalog per provider. Fields mirror models.dev schema: `reasoning`, `tool_call`, `attachment`, `structured_output`, `temperature`, `cost_*`, `limit_context`, `limit_output`, `modalities_*`, etc. Same `source` distinction as providers.

**`projects`** — Groups conversations under a name and working directory. Replaces the global `working_dir` concept.

**`conversations`** — Simplified: belongs to a project, only has `title`. No longer stores `provider_spec` or `working_dir`.

**`messages`** — Records `provider_id` and `model_id` (no foreign key constraints, no cascade delete). Tolerates missing provider/model references since models can be deleted while messages persist indefinitely.

### Key Design Choices

1. **No FK cascade on provider/model references in messages**: Messages are append-only historical records. If a provider or model is deleted, the message retains its `provider_id`/`model_id` values. Application code handles the "not found" case gracefully.

2. **`source` column (builtin vs custom)**: Builtin rows are seeded on startup and cannot be deleted. Custom rows are fully user-managed. This allows future migration to loading builtin data from models.dev JSON/TOML exports.

3. **`adapter_type` instead of `npm`**: We don't use npm package identifiers. Instead we use `adapter_type` which maps to our actual implementations: `"anthropic"` and `"openai_compatible"`.

4. **Per-message model selection**: `SendMessageRequest` accepts `provider_spec` (format: `{provider_id}:{model_id}`). No project-level or conversation-level model config. Each message can use a different model.

5. **API key stored in DB**: `providers.api_key` stores the key directly in SQLite. Currently plaintext; encryption can be added later.

### Removed Concepts

- Global `OCT_PROVIDER` and `OCT_WORKING_DIR` environment variables
- `AppState.provider_spec` and `AppState.working_dir`
- `/api/config` endpoints
- `conversations.provider_spec` and `conversations.working_dir` columns

## Consequences

- **Positive**: Full provider/model management, project grouping, per-message flexibility, clean data model aligned with models.dev
- **Negative**: API key stored in plaintext in SQLite, more complex API surface, migration breaks existing data
- **Risk**: Future models.dev integration requires upsert logic for builtin rows
