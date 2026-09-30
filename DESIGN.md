---
name: YoLab
description: Your own cloud, made simple.
colors:
  signal-blue: "#2b56f5"
  signal-blue-deep: "#1c3fd6"
  signal-blue-soft: "#eaeeff"
  ink: "#0a0a0c"
  ink-muted: "#55555f"
  ink-subtle: "#8b8b96"
  paper: "#ffffff"
  paper-soft: "#f6f6f8"
  ink-field: "#0a0a0c"
  on-ink: "#f4f4f6"
  on-ink-muted: "#a2a2ad"
  border: "#e8e8ec"
  border-strong: "#d3d3da"
  success-green: "#12a150"
typography:
  display:
    fontFamily: "Geist, ui-sans-serif, system-ui, sans-serif"
    fontSize: "clamp(2.5rem, 6.2vw, 4.25rem)"
    fontWeight: 600
    lineHeight: 1.1
    letterSpacing: "-0.03em"
  headline:
    fontFamily: "Geist, ui-sans-serif, system-ui, sans-serif"
    fontSize: "clamp(1.75rem, 3.8vw, 2.6rem)"
    fontWeight: 600
    lineHeight: 1.1
    letterSpacing: "-0.022em"
  title:
    fontFamily: "Geist, ui-sans-serif, system-ui, sans-serif"
    fontSize: "1.05rem"
    fontWeight: 600
    lineHeight: 1.2
  body:
    fontFamily: "Geist, ui-sans-serif, system-ui, sans-serif"
    fontSize: "1rem"
    fontWeight: 400
    lineHeight: 1.6
  label:
    fontFamily: "Geist Mono, ui-monospace, monospace"
    fontSize: "0.78rem"
    fontWeight: 500
    letterSpacing: "0.04em"
rounded:
  sm: "10px"
  md: "14px"
  mark: "7px"
  pill: "999px"
spacing:
  section: "clamp(3.5rem, 8vw, 7rem)"
  gap-sm: "0.5rem"
  gap-md: "1rem"
  gap-lg: "2rem"
components:
  button-primary:
    backgroundColor: "{colors.signal-blue}"
    textColor: "{colors.paper}"
    rounded: "{rounded.sm}"
    padding: "0.7rem 1.15rem"
  button-primary-hover:
    backgroundColor: "{colors.signal-blue-deep}"
  button-secondary:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
    rounded: "{rounded.sm}"
    padding: "0.7rem 1.15rem"
  chip:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink-muted}"
    rounded: "{rounded.pill}"
    padding: "0.4rem 0.85rem"
  chip-selected:
    backgroundColor: "{colors.ink}"
    textColor: "{colors.paper}"
  card:
    backgroundColor: "{colors.paper}"
    rounded: "{rounded.md}"
    padding: "1.1rem"
---

# Design System: YoLab

## Overview

**Creative North Star: "The Quiet Machine"**

A home appliance that happens to be a computer. The interface is built the way the
product is: precise, calm, and confident enough not to shout. The register is a real
product company — the bar set by 1Password, Umbrel, Linear and Vercel — where a sharp
promise is followed immediately by the actual product, honest facts, and generous space.
Nothing is decorative for its own sake: every gradient, shadow and motion earns its place
by making the product clearer or the page feel considered.

Density is low and deliberate — one idea per screen, a single primary action, long quiet
gaps between sections. Colour is a signal, not a mood: one saturated blue means "act" and
appears almost nowhere else. Light is the default because the scene is a person at home
in daylight, reading about a box they will put under a desk.

**Key Characteristics:**

- Product-led: the product surface appears before any feature list.
- One accent, used sparingly, carrying every action.
- Crisp, hairline, tight-radius surfaces; shadow is a state, not a style.
- Motion is material: an orchestrated entrance, scroll reveals, and small pointer
  responses — never scattered effects.
- The marketing site and the box's own UI are one product and share this world.

## Colors

A near-monochrome neutral field interrupted, rarely, by a single saturated blue.

### Primary

- **Signal Blue** (#2b56f5): the only saturated colour in the system. It marks the primary
  action, the active state, and the one word of emphasis in a headline. It is also the
  tint behind soft fields (`signal-blue-soft`) and the base of the aurora.
- **Signal Blue Deep** (#1c3fd6): the pressed/hover state of anything filled with Signal
  Blue. Never used at rest.
- **Signal Blue Soft** (#eaeeff): the tint behind icons, chips, and the count-up pricing
  card's glow. A background, never a text colour on white.

### Neutral

- **Ink** (#0a0a0c): body and heading text; also the fill of the dark privacy band and of
  icon tiles.
- **Ink Muted** (#55555f): secondary prose, captions, supporting lines.
- **Ink Subtle** (#8b8b96): labels, metadata, the smallest type.
- **Paper** (#ffffff): the page ground and every raised card.
- **Paper Soft** (#f6f6f8): the trust strip, the app-sidebar in the product preview, and
  inset surfaces.
- **Border** (#e8e8ec) / **Border Strong** (#d3d3da): hairlines and the stronger edge on
  interactive controls.
- **On Ink** (#f4f4f6) / **On Ink Muted** (#a2a2ad): text on the dark band.
- **Success Green** (#12a150): the "live" status dot and the check marks in pricing lists.
  It is a status colour, never a brand colour.
- **Warning Amber** (#b45309) / **Danger Red** (#dc2626): the box UI's other two status
  colours, each with a soft tint for its banner. Amber means "needs attention, will
  recover", red means "stopped until you act". Neither appears outside a status.

### Named Rules

**The One Voice Rule.** Signal Blue occupies under ~5% of any screen. Its rarity is what
makes it read as "act".

**The No Decorative Hue Rule.** Green means a running app or a satisfied condition. If a
colour is not Signal Blue, Ink, or a neutral, it is reporting state — never styling.

## Typography

**Display Font:** Geist (with ui-sans-serif, system-ui)
**Body Font:** Geist (same family; hierarchy comes from size and weight, not a second face)
**Label/Mono Font:** Geist Mono

**Character:** One workhorse grotesque doing everything, with a mono for data, labels and
numbers. The restraint is the point: a product company's page, not a type specimen. Geist
is Vercel's own face, which is why it sits correctly beside the references.

### Hierarchy

- **Display** (600, clamp(2.5rem, 6.2vw, 4.25rem), 1.1, tracking -0.03em): the single hero
  promise, and the closing call to action.
- **Headline** (600, clamp(1.75rem, 3.8vw, 2.6rem), 1.1, tracking -0.022em): section
  headings. They carry the whole argument on their own — no eyebrow, no kicker.
- **Title** (600, ~1.05rem, 1.2): card and step titles.
- **Body** (400, 1rem, 1.6): prose, capped at 44–48ch for ledes and 62ch in dense blocks.
- **Label** (500, 0.78rem, tracking 0.04em, uppercase): mono eyebrows, status text,
  metadata, and every number that should align.

### Named Rules

**The Heading Speaks Alone Rule.** No eyebrow, kicker or label sits above a heading. The
heading carries its own weight; a label above it is deleted, not restyled.

**The Mono Is Data Rule.** Geist Mono is for numbers, statuses, paths, labels and code. It
is never a costume for "technical".

## Layout

A single centred container, `72rem` wide, with `clamp(1.1rem, 4vw, 2rem)` inline padding.
Sections are separated by `clamp(3.5rem, 8vw, 7rem)` of vertical space, so the page reads
as a sequence of quiet, generous rooms rather than a stack of bands. The hero is centred
and full-width; the product surface sits directly beneath the promise at full container
width, which is the page's core move.

Content grids are `auto-fill` with a `minmax` floor — `15rem` for catalogue cards, `20rem`
for use-case cards, `12rem` for the trust strip — so columns reflow without breakpoint
churn. The only fixed breakpoints are structural: `60rem` (nav collapses to a menu),
`62rem` (two-column sections stack), `46rem` (the product preview drops its sidebar).

## Elevation & Depth

A hybrid, leaning flat. The page ground is flat paper; depth appears only where a surface
is genuinely raised above it (cards, the product frame, the price card) or as a response
to state (a card under the pointer). There is no ambient shadow on resting text or bands.

### Shadow Vocabulary

- **card** (`0 1px 2px rgb(9 9 11 / 0.05), 0 1px 3px rgb(9 9 11 / 0.04)`): the resting
  lift of a card or chip.
- **lift** (`0 6px 16px -4px rgb(9 9 11 / 0.1), 0 2px 6px -2px rgb(9 9 11 / 0.05)`): the
  hover state of a card, and the resting state of the product frame.
- **pop** (`0 32px 64px -16px rgb(9 9 11 / 0.22)`): reserved for the product surface, the
  single object on the page that should feel physically present.

### Named Rules

**The Flat-By-Default Rule.** Surfaces are flat at rest. A shadow is either structural
(this card is raised) or a state (the pointer is on it) — never decoration.

## Shapes

Tight, technical corners. Cards and panels use a `14px` radius; buttons, inputs and small
surfaces `10px`. Pills (`999px`) are reserved for filter chips and
status badges. Borders are always `1px` hairlines, `border` at rest and `border-strong` on
interactive controls — never a coloured left or right accent bar.

## Components

### Buttons

- **Shape:** 10px radius (`--r-s`), no shadow at rest.
- **Primary:** Signal Blue fill, paper text, `0.7rem 1.15rem` padding; `--lg` variant at
  `0.85rem 1.5rem`, `--sm` at `0.5rem 0.85rem`.
- **Hover / Focus:** background shifts to Signal Blue Deep; the trailing arrow translates
  `2px`. Focus is a 2px Signal Blue outline at 2px offset.
- **Secondary:** paper fill, ink text, `border-strong` hairline; hover darkens the border.
- **Ghost:** no fill, muted text; hover moves to ink. Used for tertiary links only.

### Chips (filters)

- **Style:** pill, paper fill, `border-strong` hairline, muted text.
- **State:** selected inverts to Ink fill with paper text — the filter row's active state
  is the only place Ink is used as a fill outside the dark band.

### Cards / Containers

- **Corner Style:** 14px radius.
- **Background:** Paper; the dark band is Ink.
- **Shadow Strategy:** `card` at rest, `lift` on hover (see Elevation).
- **Border:** 1px `border`.
- **Internal Padding:** `1.1rem` for catalogue cards, `clamp(1.2rem, 3vw, 1.8rem)` for
  pricing rows.

### Navigation

Sticky, paper at 86% with a `backdrop-filter` blur and saturation, closed by a 1px
hairline; a shadow appears once the page scrolls. Links are muted at rest, ink on hover,
with a soft ink wash behind the hovered item. Below `60rem` the links collapse into a
full-width sheet behind a single Menu button — the nav never disappears without a
replacement.

### Logo

The mark is a house with its cloud parked in front of it. It is only ever placed from
the outlined files in `brand/yolab-logo` (horizontal lockup in navigation, symbol where
space is square, the no-door small symbol below 32px, reversed on dark grounds), never
retyped, recoloured or rebuilt in CSS. Clear space, minimum sizes and approved pairs are in
that kit's `GUIDELINES.md`.

### Product Surface (signature)

The console preview — a browser frame containing the real product layout (sidebar, app
grid, live status dots). It is the page's proof and its most important object: full
container width, `pop` shadow, a pointer tilt of up to 4°, and a staggered arrival of its
app tiles after the frame lands. Real screenshots replace the preview through the `tour`
media slot without any layout change.

## App UI (the box's own screens)

The marketing site persuades; the box UI (`homelab/client-ui`) is where people operate
their home server. It keeps every token above and adds one rule for all of it: **the same
thing always looks the same.** A screen is assembled from the few parts below, never from
one-off markup. If a screen needs something this section does not name, the section is
extended first.

### Page anatomy

Every page follows one order, top to bottom:

1. **Back link** (only on detail pages): muted text, arrow icon.
2. **Header**: the page's name in the Headline style at app scale, an optional one-line
   status or subtitle beneath it, and **at most one primary action** on the right.
3. **One status banner**, when something needs attention (see Status).
4. **Sections**, in order of how often people need them.
5. **Advanced**: technical detail behind a single disclosure row.
6. **Remove / destructive section**: always last, always alone.

Sections are separated by one fixed gap (`2rem`). There are no other vertical spacings
between blocks.

### Sections and rows

- **Section**: a heading *outside* the card, `text-sm`, 600, `ink-muted`, no icon, no
  eyebrow; one optional quiet action on the heading's right. The body is one card of
  **rows separated by hairlines**. Cards are never nested and never used as spacers.
- **Row**: label (ink, 500) with an optional detail line (ink-muted) on the left, and one
  trailing slot on the right. The trailing slot holds exactly one of: a value, a switch,
  a select, a quiet row action, or a chevron (the whole row is the button). Rows are
  `1rem 1.25rem` padded.
- **Value rows**: values that are data (addresses, versions, IDs, paths, numbers) are in
  Geist Mono. Copyable values carry the one shared copy button; secrets carry the one
  shared reveal button.
- **Navigation rows** (chevron) open a page or a sheet. A row that opens something
  external ends in the external-link icon instead.

### Actions

- **Primary**: one filled Signal Blue button per screen, for the thing the screen exists
  for (Install, Sign in, Continue). A page that is about one thing, like an app's page,
  may have none: its addresses are rows in Access. Never for Save, never for a secondary
  task.
- **Row action**: quiet blue text inside the row it affects ("Back up now",
  "Update to 1.3"). Actions live next to what they change; there are no free-floating
  button rows.
- **Secondary buttons**: paper fill, `border-strong` hairline, ink text. Only in sheets,
  dialogs and banners.
- **Destructive**: a row in red text in the last section, which opens a confirmation.
  The filled red button exists only inside that confirmation.
- **Icon buttons**: one style (muted icon, `surface-2` wash on hover, visible focus
  outline). Copy and reveal are the same component everywhere.
- **Availability follows state**: an action that means nothing in the current state is
  not shown (a failed install offers "Try again" and "Remove", nothing else; an app
  being removed offers nothing).
- All buttons: 10px radius; hover deepens (Signal Blue Deep), never lightens; focus is a
  2px Signal Blue outline at 2px offset.

### Status

Status colours report state and never decorate:

| State | Tone | Where |
|---|---|---|
| Running | Success Green | dot in the header status line; nothing on tiles |
| Starting, copying, installing, restoring | Signal Blue (pulse) | dot + status line; banner only when it takes minutes |
| Needs attention, stopped working, will retry | Warning Amber | dot + banner with the reason |
| Failed, cannot proceed without you | Danger Red | dot + banner with the reason **and** the actions that fix it |
| Finished a long task (restored, updated) | Success Green | one success banner, dismissible |

- **One banner per screen.** When several apply, the highest wins: removing → failed →
  restoring → stopped → finished → starting.
- A red banner always carries its way out as buttons inside it; "scroll down to…" is
  never the instruction.
- Every banner is announced (`role="status"`, `role="alert"` for red).

### Forms and saving

- On/off is always a switch, never a checkbox.
- Choices start from named presets in a select; the raw form (a cron expression, a path)
  appears only after choosing "Custom…".
- Switches and selects **save on change** and show a quiet inline "Saved". An explicit
  Save button exists only for free text, sits beside that field, and is secondary.
- Long operations show their progress where they were started and end in a visible
  finished state; silence is never an outcome.

### Advanced

IDs, installed settings, running processes and logs live behind one "Technical details"
row that expands in place. Nothing technical appears above it.

## Do's and Don'ts

### Do:

- **Do** lead every section with a heading that states the argument on its own.
- **Do** keep Signal Blue to actions, active states, and one emphasised word.
- **Do** show the product before claiming anything about it.
- **Do** put numbers, statuses and paths in Geist Mono, with tabular figures.
- **Do** use shadow only as structure or state, and keep resting surfaces flat.
- **Do** keep the marketing site and the box UI on the same tokens — they are one product.

### Don't:

- **Don't** put an eyebrow, kicker or label above a heading.
- **Don't** use gradient text; emphasis comes from weight or size.
- **Don't** use glass or blur as decoration — only the sticky nav's backdrop.
- **Don't** add a coloured `border-left` or `border-right` to cards or callouts.
- **Don't** introduce a second accent hue; green reports status, nothing else.
- **Don't** let an animation hide content: reveals are gated on JS, and anything below the
  fold is visible by default.
