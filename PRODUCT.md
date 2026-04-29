# Product

## Register

product

## Users

Developers working at their desks in long, focused sessions. They have Oct open alongside terminals, editors, and browsers. Their primary context is a coding task: reading code, asking the agent to write or refactor something, reviewing tool execution output (file reads, writes, shell commands), and iterating. Some users have visual impairments requiring larger body text (20px+) and enhanced contrast beyond typical defaults.

## Product Purpose

Oct is a local AI coding agent with a multi-provider LLM backend. It executes code tasks through a chat interface: reading and writing files, running commands, and streaming results back in real time. Success means the agent feels like a capable pair programmer that disappears into the workflow, not a chatbot you talk to.

## Brand Personality

Calm, capable, precise. The interface stays out of the way and trusts the developer to drive. No enthusiasm, no celebration, no decoration. Output is clear and legible above all else.

## Anti-references

- Not a ChatGPT clone: no centered chat bubble layout, no per-message avatars, no conversational decoration.
- Not enterprise SaaS: no dense dashboards, no data table grids, no corporate blue.
- Not a terminal emulator: no green-on-black, no monospace-everything, no hacker aesthetic.

## Design Principles

1. **Legibility over everything.** Body text at 20px+, high contrast, generous line height. If a design choice makes text harder to read, it's wrong.
2. **Tool, not conversation.** The chat is a command surface. Optimize for scanning output, reading code diffs, and tracking tool execution, not for back-and-forth dialogue.
3. **Quiet confidence.** No decorative motion, no visual celebration, no empty states that perform personality. The interface is present when needed and invisible when not.
4. **Structured density.** Show related information together. Tool output, file paths, and command results live in context, not buried in scroll. Dense when useful, sparse when clarity demands it.
5. **Consistent affordances.** Same button, same form control, same state vocabulary everywhere. A developer should never second-guess how to interact.

## Accessibility & Inclusion

- WCAG AAA contrast ratios (7:1 for normal text, 4.5:1 for large text) as the baseline.
- Body text minimum 20px (1.25rem). Labels and secondary text minimum 16px (1rem).
- Respect `prefers-reduced-motion`: disable all decorative transitions, keep only essential state-change feedback under 150ms.
- No reliance on color alone to convey meaning: pair color with text labels or icons.
- Keyboard-navigable for all interactive elements with visible focus indicators.
