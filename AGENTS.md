# AGENTS.md

## Project Structure

```
src/
  core/           # Core domain types (Message, Tool, Usage, StreamEvent, Error)
  model/          # Traits: ChatModel, EmbeddingModel
  provider/       # Provider trait, Registry, ModelInfo, Capabilities, Limits
  adapter/        # Protocol adapters (OpenAI-compatible)
  providers/      # Concrete providers (Kimi, Anthropic)

tests/            # Integration tests and fixtures
  fixtures/       # JSON fixtures for snapshot tests
  snapshots/      # Insta snapshot files (*.snap)
```

## Architecture Notes

- **Provider abstraction**: `Provider` trait creates `ChatModel` and `EmbeddingModel` instances
- **Wire protocol mapping**: Adapters convert between internal types (`Message`, `ToolCall`) and provider-specific JSON
- **Streaming**: SSE parsing with tool call state accumulation using `index` as key