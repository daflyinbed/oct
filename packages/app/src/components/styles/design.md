# Theme System Documentation

A comprehensive color and theming infrastructure built with CSS custom properties for consistent, flexible component styling.

## 📋 Table of Contents

- [Core Concepts](#core-concepts)
- [Theme Structure](#theme-structure)
- [Usage](#usage)
- [Variables Reference](#variables-reference)

## Core Concepts

### CSS Custom Properties

The theme system uses CSS custom properties (CSS variables) as the foundation for all theming capabilities. This allows for:

- **Runtime theme switching** - Change themes without reloading
- **Component-level customization** - Theme individual components
- **Brand customization** - Easy brand color updates

### Color System Architecture

The color system is organized into two main categories:

1. **Background Colors**: Background, Panel, Surface, Overlay
2. **Semantic Colors**: Seven color groups with eleven variants each

## Theme Structure

### Root Variables

All theme variables are defined at the `:root` level for global access:

```css
:root {
  /* Background Colors */
  --color-background: #fff;
  --color-panel: #ffffffb3;
  --color-surface: #ffffffd9;
  --color-overlay: #0006;

  /* Border & Effects */
  --border: 1px; /* Border size */

  /* Semantic Colors - Accent */
  --color-accent-1: #f7f9ff;
  --color-accent-2: #edf2fe;
  --color-accent-3: #e1e9ff;
  --color-accent-4: #d2deff;
  --color-accent-5: #c1d0ff;
  --color-accent-6: #abbdf9;
  --color-accent-7: #8da4ef;
  --color-accent-8: #3e63dd;
  --color-accent-9: #3358d4;
  --color-accent-10: #3a5bc7;
  --color-accent-contrast: #fff;

  /* Additional semantic color groups... */
}
```

### Configuration Guidelines

## Usage

### Theme Application Methods

#### 1. HTML Data Attributes

Use `data-theme` attribute for simple, declarative theming:

```html
<!-- Component-level theming -->
<button data-theme="dark">Dark Button</button>

<!-- Section-level theming -->
<div data-theme="light">
  <h1>Light Section</h1>
  <p>All child elements inherit this theme</p>
</div>

<!-- Page-level theming -->
<body data-theme="dark">
  <!-- Entire page uses dark theme -->
</body>
```

#### 2. CSS Theme Definitions

Define themes using CSS attribute selectors:

```css
/* Light Theme */
[data-theme="light"] {
  --color-background: #fff;
  --color-panel: #ffffffb3;
  --color-surface: #ffffffd9;
  --color-overlay: #0006;
}

/* Dark Theme */
[data-theme="dark"] {
  /* ... variables */
}
```

## Variables Reference

### 🎨 Background Colors

| Variable             | Purpose                                                             |
| -------------------- | ------------------------------------------------------------------- |
| `--color-background` | Main background                                                     |
| `--color-panel`      | Panel backgrounds, such as cards, tables, popovers, dropdown menus  |
| `--color-surface`    | Form component backgrounds, such as text fields, checkboxes, select |
| `--color-overlay`    | Dialog overlays                                                     |

### 🎯 Semantic Color Groups

Each group provides 11 coordinated variants:

#### Structure: `--color-{group}-{[1-10]|constract}

**Variants:**

- `-1` - Outline and ghost hover background
- `-2` - Outline and ghost active background，Surface background，disabled background(neutral color)
- `-3` - Surface hover background
- `-4` - Surface active background
- `-5` - Outline and ghost border
- `-6` - Outline and ghost hover border，disabled border color(neutral color)
- `-7` - Solid background, disabled text color(neutral color)
- `-8` - Solid hover background
- `-9` - Solid active background
- `-10` - Text color
- `-contrast` - Text color on solid

#### Available Groups

1. **Accent** - Main brand colors
2. **Neutral** - Neutral colors
3. **Info** - Informational colors
4. **Success** - Success states
5. **Warning** - Warning states
6. **Error** - Error states
