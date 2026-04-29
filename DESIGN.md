---
name: Oct
description: Local AI coding agent with calm, capable precision
colors:
  accent-9: "#3a5bc7"
  accent-7: "#3e63dd"
  accent-contrast: "#ffffff"
  neutral-bg: "#f8f8f5"
  neutral-panel: "#f2f1ed"
  neutral-surface: "#eae8e3"
  neutral-text: "#1c1b18"
  neutral-muted: "#6b6960"
  neutral-border: "#d4d2cb"
  success-7: "#16a34a"
  warning-7: "#d97706"
  error-7: "#dc2626"
  info-7: "#0284c7"
typography:
  display:
    fontFamily: "Inter, system-ui, sans-serif"
    fontSize: "2rem"
    fontWeight: 600
    lineHeight: 1.2
  headline:
    fontFamily: "Inter, system-ui, sans-serif"
    fontSize: "1.5rem"
    fontWeight: 600
    lineHeight: 1.3
  title:
    fontFamily: "Inter, system-ui, sans-serif"
    fontSize: "1.25rem"
    fontWeight: 500
    lineHeight: 1.4
  body:
    fontFamily: "Inter, system-ui, sans-serif"
    fontSize: "1.25rem"
    fontWeight: 400
    lineHeight: 1.6
  label:
    fontFamily: "Inter, system-ui, sans-serif"
    fontSize: "1rem"
    fontWeight: 500
    lineHeight: 1.4
    letterSpacing: "0.02em"
  mono:
    fontFamily: "\"JetBrains Mono\", \"Fira Code\", ui-monospace, monospace"
    fontSize: "0.9em"
    fontWeight: 400
    lineHeight: 1.5
rounded:
  1: "3px"
  2: "4px"
  3: "6px"
  4: "8px"
  5: "12px"
  6: "16px"
spacing:
  1: "4px"
  2: "8px"
  3: "12px"
  4: "16px"
  5: "24px"
  6: "32px"
  7: "40px"
  8: "48px"
  9: "64px"
components:
  button-primary:
    backgroundColor: "{colors.accent-9}"
    textColor: "{colors.accent-contrast}"
    rounded: "{rounded.3}"
    padding: "8px 20px"
  button-primary-hover:
    backgroundColor: "{colors.accent-7}"
  button-ghost:
    backgroundColor: "transparent"
    textColor: "{colors.neutral-text}"
    rounded: "{rounded.3}"
    padding: "8px 20px"
  button-ghost-hover:
    backgroundColor: "{colors.neutral-panel}"
  input-default:
    backgroundColor: "{colors.neutral-surface}"
    textColor: "{colors.neutral-text}"
    rounded: "{rounded.3}"
    padding: "10px 14px"
---

# Design System: Oct

## 1. Overview

**Creative North Star: "The Workbench"**

A well-organized workbench where every tool has its place and nothing is decorative. The surface is warm and grounded, the layout is structured, and the primary accent signals capability without demanding attention. Depth comes from tonal layering: background, panel, and surface read as three distinct planes. Shadows appear only when the user interacts, as tactile feedback that something responded.

This system rejects the conversational decoration of ChatGPT-style chat bubbles, the corporate density of enterprise SaaS dashboards, and the nostalgic grit of terminal emulators. Oct is a tool for developers who work long hours: it respects their eyes with large text and high contrast, and it respects their attention by staying quiet until needed.

**Key Characteristics:**
- Warm neutral surfaces with cool slate-blue accent for primary actions
- Body text at 1.25rem (20px) minimum, WCAG AAA contrast
- Flat surfaces at rest, shadow on interaction only
- Tactile, confident controls with clear interactive boundaries
- Monospace for code paths, tool output, and file references

## 2. Colors

A warm neutral foundation with a single cool accent. The warm undertone in backgrounds and text prevents the clinical coldness common to developer tools, while the deep slate blue carries interactive authority.

**The One Accent Rule.** The accent color appears on primary actions, active states, and focus indicators only. It must never exceed 10% of any screen's visual weight. Its rarity is the point: when it appears, it means "act here."

### Primary
- **Deep Slate Blue** (#3a5bc7 / oklch(44% 0.15 270)): Primary interactive accent. Used on primary button fills, active navigation items, focus rings, and selected states. Deliberately desaturated relative to pure blue to avoid the AI-tool cliché.

### Neutral
- **Warm Ivory** (#f8f8f5 / oklch(98% 0.004 85)): Main background. Warm enough to read as paper, not clinical white. Every surface is tinted toward yellow-ochre.
- **Warm Linen** (#f2f1ed / oklch(96% 0.005 85)): Panel background. Sidebars, cards, and grouped content sit on this layer, one tonal step above the background.
- **Warm Sand** (#eae8e3 / oklch(94% 0.006 85)): Surface and input background. Form fields, code blocks, and inset areas use this third tonal step.
- **Warm Charcoal** (#1c1b18 / oklch(14% 0.01 85)): Primary text. Near-black but tinted warm, never pure black.
- **Warm Stone** (#6b6960 / oklch(48% 0.01 85)): Secondary and muted text. Labels, timestamps, metadata.
- **Warm Border** (#d4d2cb / oklch(86% 0.006 85)): Borders and dividers. Visible enough to define structure, never attention-grabbing.

### Semantic
- **Success** (#16a34a / oklch(52% 0.15 155)): Tool execution success, completed states.
- **Warning** (#d97706 / oklch(60% 0.15 75)): Caution states, pending actions.
- **Error** (#dc2626 / oklch(50% 0.18 25)): Failed tool execution, validation errors.
- **Info** (#0284c7 / oklch(48% 0.13 235)): Informational states, neutral notifications. Distinct from the primary accent in hue and purpose.

**The Tonal Layer Rule.** Background, panel, and surface form a three-step tonal ramp in the same warm hue. No neutral jumps more than one step from its neighbor. If two adjacent surfaces are the same lightness, the hierarchy has failed.

## 3. Typography

**Body Font:** Inter (with system-ui, sans-serif fallback)
**Mono Font:** JetBrains Mono (with Fira Code, ui-monospace fallback)

**Character:** A single sans-serif family at a generous size. Inter carries headings, body, and labels with weight contrast alone. Monospace enters only for code: file paths, tool output, and inline code references. No display font, no serif, no pairing. One voice, many weights.

### Hierarchy
- **Display** (600, 2rem/32px, line-height 1.2): Page titles only. Rare in a tool; reserve for the app shell's project name.
- **Headline** (600, 1.5rem/24px, line-height 1.3): Section headings within panels. Conversation titles, settings groups.
- **Title** (500, 1.25rem/20px, line-height 1.4): Subsection labels, list item headers, conversation titles in sidebars.
- **Body** (400, 1.25rem/20px, line-height 1.6): All running text, chat messages, tool output prose. 65-75ch max line length. This is the minimum: nothing in the primary content area falls below 20px.
- **Label** (500, 1rem/16px, letter-spacing 0.02em): Button text, form labels, badges, metadata. The only element permitted below 20px, and only for non-prose UI chrome.
- **Mono** (400, 0.9em relative, line-height 1.5): Code paths, terminal output, file names. Scales relative to its parent: at 0.9em inside body (20px), it renders at 18px.

**The Scale Integrity Rule.** The ratio between adjacent type steps is at least 1.2. Headline is 1.33x body. Body is 1.25x label. Flat scales where everything is 16-18px are prohibited.

## 4. Elevation

Flat surfaces at rest. Depth is conveyed through the three-step tonal layer (background, panel, surface). Shadows appear exclusively as interaction feedback: hover on interactive elements, focus on inputs, and transient states like drag. They disappear the moment the interaction ends.

**The Interaction-Only Shadow Rule.** If a shadow is visible when the user is not actively interacting with that element, the shadow is decoration. Remove it.

### Shadow Vocabulary
- **Hover lift** (`0 2px 8px rgba(28, 27, 24, 0.08)`): Appears on button hover, card hover. Diffuse, warm-tinted, subtle.
- **Focus ring** (`0 0 0 2px var(--color-accent-9)`): Focus indicator on inputs, buttons, and interactive elements. Uses the primary accent, not a shadow.
- **Active press** (`none`): Shadow removed on active/pressed state. The element returns flush to the surface.

## 5. Components

Tactile and confident. Every interactive element has clear boundaries and unambiguous state feedback. No ghost borders, no ambiguous hover states, no decorative rounding.

### Buttons
- **Shape:** Gently curved (6px radius)
- **Primary:** Deep Slate Blue fill (#3a5bc7) with white text, 8px 20px padding. Weight 500 at label size.
- **Primary Hover:** Shifts to Accent-7 (#3e63dd), hover-lift shadow appears.
- **Primary Active:** Shadow removed, fill stays at Accent-7.
- **Ghost:** Transparent background, Warm Charcoal text, same padding and radius.
- **Ghost Hover:** Warm Linen (#f2f1ed) background fill.
- **Focus:** 2px accent ring on all button variants.

### Inputs / Fields
- **Shape:** Same radius as buttons (6px). Visual consistency with controls.
- **Background:** Warm Sand (#eae8e3). The third tonal step reads as an inset surface.
- **Text:** Warm Charcoal at body size (1.25rem).
- **Border:** 1px Warm Border (#d4d2cb) at rest.
- **Focus:** Border replaced by 2px accent ring, background lightens to Warm Linen.
- **Error:** Border shifts to Error-7, error message below in Error-7 text.

### Navigation
- **Sidebar:** Warm Linen (#f2f1ed) background, the panel tonal layer. Conversation list items are ghost-style: transparent at rest, Warm Sand on hover, accent text + left indicator on active.
- **Top bar:** Warm Ivory (#f8f8f5), the background layer. Minimal: project name, global actions.

### Code / Tool Output
- **Background:** Warm Sand (#eae8e3) with monospace font at 0.9em relative size.
- **Inline code:** Warm Sand background with 3px radius, 2px horizontal padding.
- **Block code:** Full-width Warm Sand panel with 6px radius and 16px padding.

### Cards / Containers
- **Background:** Warm Linen (#f2f1ed), the panel layer.
- **Border:** None at rest. Container identity comes from tonal difference with the background, not from strokes.
- **Shadow:** None at rest. Hover-lift shadow only if the card is interactive.
- **Internal Padding:** 16px (spacing-4) standard, 24px (spacing-5) for spacious sections.

## 6. Do's and Don'ts

### Do:
- **Do** use body text at 1.25rem (20px) minimum in all primary content areas.
- **Do** tint every neutral toward the warm ochre hue (chroma 0.004-0.01 in OKLCH).
- **Do** use the three-step tonal layer (background, panel, surface) for depth instead of shadows.
- **Do** add hover-lift shadow only on interactive elements during hover, and remove it on active/press.
- **Do** use monospace exclusively for code paths, file names, and tool output.
- **Do** ensure WCAG AAA contrast (7:1) between text and its background.
- **Do** pair semantic colors with text labels or icons, never rely on color alone.
- **Do** show focus rings (2px accent) on all interactive elements for keyboard navigation.

### Don't:
- **Don't** use centered chat-bubble layouts, per-message avatars, or conversational decoration. (PRODUCT.md: "Not a ChatGPT clone")
- **Don't** use corporate blue, dense data-table grids, or SaaS dashboard patterns. (PRODUCT.md: "Not enterprise SaaS")
- **Don't** use green-on-black, monospace-everywhere, or hacker-terminal aesthetics. (PRODUCT.md: "Not a terminal emulator")
- **Don't** show shadows on resting elements. If it's not being interacted with, it's flat.
- **Don't** use pure #000 or #fff. Every neutral must carry the warm tint.
- **Don't** use border-left or border-right greater than 1px as a colored stripe accent on cards, list items, or callouts.
- **Don't** use gradient text (background-clip: text with gradient background).
- **Don't** use glassmorphism, blurs, or frosted-glass cards as default decoration.
- **Don't** set text below 1rem (16px) anywhere, and below 1.25rem (20px) in primary content.
- **Don't** use the accent color on more than 10% of any screen's visual weight.
