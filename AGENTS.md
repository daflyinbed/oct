# AGENTS.md

## Monorepo Layout

Two separate package managers coexist:

- **Rust workspace** (`Cargo.toml`): `crates/oct-llm-provider` (library) → `crates/oct-agent` (binary)
- **pnpm workspace** (`pnpm-workspace.yaml`): `packages/app` (Vue 3 frontend)

## Commands

### Rust (run from workspace root)

```bash
cargo build -p oct-agent              # build the backend binary
cargo run -p oct-agent                 # run backend `serve` subcommand (port 3000 by default, override with OCT_PORT)
cargo run -p oct-agent -- serve        # explicit serve subcommand
cargo run -p oct-agent -- seed         # seed providers & models from models.dev API into the database
cargo test -p oct-llm-provider         # run library tests (snapshot tests via insta)
cargo test -p oct-llm-provider --test anthropic  # single integration test file
cargo sqlx prepare --workspace         # regenerate .sqlx/ cache
```

### Frontend (run from workspace root with pnpm)

```bash
pnpm --filter frontend dev            # Vite dev server (port 5173)
pnpm --filter frontend build          # vue-tsc typecheck then vite build
pnpm --filter frontend generate-api   # regenerate TypeScript types from OpenAPI schema (requires running server)
```

### Lint

```bash
pnpm -r exec eslint .                 # ESLint with @xwbx/eslint-config flat config
```

### Infrastructure

```bash
docker compose up -d                  # start PostgreSQL 18.3 (port 5432, user/pass/db: oct)
```

## Build & Database

- **Database**: PostgreSQL (via `docker-compose.yml`). Default `DATABASE_URL`: `postgres://oct:oct@localhost:5432/oct` (override via env var or `.env`)
- **sqlx compile-time macros**: All queries use `sqlx::query!`/`query_as!`/`query_scalar!` (compile-time checked, `postgres` feature)
- **Offline mode**: `.sqlx/` cache is committed; builds work without a live database via `SQLX_OFFLINE=true`
- **Regenerate cache**: After changing SQL queries, run `cargo sqlx prepare --workspace`
- **Migrations**: `sqlx::migrate!("../../migrations")` in `crates/oct-agent/src/db/mod.rs` auto-runs on startup; migration files live at repo root `migrations/`
- **Seeding**: `cargo run -p oct-agent -- seed` fetches the model catalog from `https://models.dev/api.json` and upserts providers/models (source=`seeded`). Custom providers/models (source=`custom`) are preserved.

## CLI Subcommands

`oct-agent` has two subcommands (via `clap`):

1. **`serve`** (default): Start the API server. Options: `--port` / `OCT_PORT` (default 3000)
2. **`seed`**: Fetch providers and models from models.dev and upsert into the database

## Architecture

- **Provider abstraction** (`oct-llm-provider`): `Provider` trait creates `ChatModel`. `ProviderRegistry` resolves models by `provider:model_id` spec and supports dynamic custom OpenAI-compatible providers via `resolve_custom_openai_model()`. Adapters (`adapter/openai_compatible.rs`) convert internal types to provider-specific JSON. Concrete providers: Anthropic, MoonshotAI (Kimi).
- **Provider registry** (`oct-llm-provider/provider/registry.rs`): `ProviderRegistry` holds registered providers, resolves chat model specs, and can create ad-hoc OpenAI-compatible chat models for custom providers at runtime.
- **Agent loop** (`oct-agent/agent/`): Streams LLM responses, executes tool calls, feeds results back. Supports cancellation via `tokio_util::sync::CancellationToken`. No iteration limit.
- **Agent tools** (`oct-agent/tools/`): `read`, `write`, `listdir`, `execute`
- **API** (`oct-agent/api/`): Axum + utoipa OpenAPI generation. Scalar UI at `/scalar`. OpenAPI JSON at `/api/openapi.json`. SSE streaming for chat. Routes:
  - **Providers & Models**: CRUD for providers and models (list, create, update, delete). Models support rich metadata (capabilities, limits, costs, modalities).
  - **Projects**: CRUD (list, create, get, update, delete). Projects have `name` and `working_dir`.
  - **Conversations**: CRUD (list, create, get, update, delete). Conversations belong to a project.
  - **Chat**: Send message (SSE streaming), get messages.
- **AppState**: Holds `PgPool`, `Arc<ProviderRegistry>`, and `Arc<DashMap<String, RunHandle>>` for tracking active agent sessions.
- **Frontend** (`packages/app`): Vue 3 + Vite 8 + Tailwind v4. Uses `openapi-fetch` to consume the generated OpenAPI schema. `@vueuse/core` for composables. `unplugin-vue-components` for auto-imports. Key directories:
  - `src/composables/`: `useChat.ts`, `useProjects.ts`, `useProviders.ts`
  - `src/components/chat/`, `diff/`, `file-tree/`, `settings/`, `sidebar/`
  - `src/api/`: `client.ts` (openapi-fetch), `schema.d.ts` (auto-generated)

## Database Schema

5 tables (see `migrations/20260417182650_init_schema`):

- `providers`: id, name, adapter_type, base_url, api_key, doc_url, source (`seeded`/`custom`), timestamps
- `models`: id, provider*id, model_id, name, family, reasoning, tool_call, attachment, structured_output, temperature, knowledge, release_date, open_weights, cost*\_, limit\_\_, modalities\_\*, source, is_enabled, timestamps
- `projects`: id, name, working_dir, timestamps
- `conversations`: id, project_id, title, timestamps
- `messages`: id, conversation_id, role, parts_json, ordering, provider_id, model_id, input_tokens, output_tokens, reasoning_tokens, created_at

## Testing

- `oct-llm-provider` tests use **insta** snapshot testing (YAML). Fixtures are JSON files under `tests/<provider>/fixtures/`. After intentional output changes, run `cargo insta review` to accept new snapshots. Test files: `adapter_openai_compatible.rs`, `providers_anthropic.rs`, `registry.rs`, `anthropic/`, `moonshot/`.
- `oct-agent` has no automated tests yet.

## Code Style

- **Comments**: 除非用户明确要求，否则不要删除代码中的注释；但可以更新已有注释以保持准确性
- **Rust edition 2024**: Both crates use `edition = "2024"`
- **Path alias**: Frontend uses `@` → `packages/app/src` (Vite resolve alias)
