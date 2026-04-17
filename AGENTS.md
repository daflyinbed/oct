# AGENTS.md

## Project Structure

```
Cargo.toml                          # Workspace root
.env                                # DATABASE_URL for sqlx
dev.db                              # SQLite database (gitignored)
migrations/                         # SQLite schema migrations
.sqlx/                              # sqlx offline query cache (commit this)

crates/
  oct-llm-provider/                 # LLM provider abstraction library
    src/
      core/           # Core domain types (Message, Tool, Usage, StreamEvent, Error)
      model/          # Traits: ChatModel, EmbeddingModel
      provider/       # Provider trait, Registry, ModelInfo, Capabilities, Limits
      adapter/        # Protocol adapters (OpenAI-compatible)
      providers/      # Concrete providers (Kimi, Anthropic)
    tests/            # Integration tests and fixtures
    examples/         # Example CLI chat application

  oct-agent/                        # Coding agent application
    src/
      tools/          # Agent tools: read_file, list_dir, write_file, execute_command
      agent/          # Agent loop, system prompt, AgentEvent types
      db/             # SQLite persistence (conversations, messages, usage)
      api/            # Axum HTTP API with utoipa OpenAPI generation
      main.rs         # Entry point

frontend/                           # Vue 3 + TypeScript UI
  src/
    components/
      chat/           # ChatView, MessageList, MessageBubble, InputBar
      layout/         # Header, Sidebar
      ui/             # shadcn-vue components
    composables/      # useChat, useConversations
    types/            # TypeScript type definitions
    lib/              # API client, utilities

scripts/              # Dev and build scripts
```

## Architecture Notes

- **Provider abstraction**: `Provider` trait creates `ChatModel` and `EmbeddingModel` instances
- **Wire protocol mapping**: Adapters convert between internal types (`Message`, `ToolCall`) and provider-specific JSON
- **Streaming**: SSE parsing with tool call state accumulation using `index` as key
- **Agent loop**: Infinite loop (no iteration limit) that streams LLM responses, executes tool calls, and feeds results back
- **API**: Axum server with utoipa OpenAPI docs (Scalar UI at /scalar)
- **Frontend**: Uses openapi-fetch to consume generated OpenAPI schema; SSE streaming for real-time chat

## Build & Database

- **sqlx compile-time macros**: All queries use `sqlx::query!`/`query_as!`/`query_scalar!` (compile-time checked)
- **Offline mode**: `.sqlx/` cache is committed; builds work without a live database via `SQLX_OFFLINE=true`
- **Regenerate cache**: After changing SQL queries, run `cargo sqlx prepare --workspace` from workspace root (requires DATABASE_URL in `.env` pointing to a migrated database)
- **Migrations**: `sqlx migrate run` or run the app (auto-migrates on startup via `sqlx::migrate!`)