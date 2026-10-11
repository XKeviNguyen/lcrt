# Themes

LILOPOP has three themes, chosen in **Settings → Appearance → Theme**:

| Theme | Character | Style |
| --- | --- | --- |
| **Pop Studio** (default) | Deep purple with pink and yellow accents | dark |
| Calm Native | Light and quiet, close to the Ubuntu desktop | light |
| Cinema Focus | Dark and low-distraction, for long videos | dark |

A choice applies at once to every LILOPOP window, is saved with the other
preferences, and never restarts the session, the speech model or audio
capture.

## What a theme changes

The caption window is both LILOPOP's main window and the caption surface,
so it is split into the controls (always themed) and the captions (themed
only on request).

| Surface | Follows | Notes |
| --- | --- | --- |
| Settings window, its pages, lists, switches, menus and dialogs | Theme | libadwaita colors are overridden, so standard widgets follow without per-widget rules. |
| Caption window control bar (mode, source, language, lane chips, Start/Stop, status, Settings) | Theme | The bar has its own theme-colored surface, so it reads the same over any caption background. |
| Error banner | Desktop error colors | Solid red banner with white text (6.1:1) and, when useful, an **Open Settings** button. |
| Caption text, font, size, background color, opacity | Caption style | Never themed. |
| Caption title bar, language badges, panel outline | Caption style, or theme when **Apply theme to caption overlay too** is on | The title bar keeps the caption background opacity in both cases. |
| Status, notice and title text around the captions | Caption text color | So it reads wherever the captions do. |

The Preferences entry stores one typed value, `appearance.theme`
(`pop_studio`, `calm_native` or `cinema_focus`), plus
`appearance.apply_theme_to_overlay` (default off). A missing value means
Pop Studio. An unknown or malformed value also falls back to Pop Studio,
for that field only: the rest of the file still loads.

**Reset theme to Pop Studio** changes only the theme. **Reset caption style
to defaults** (formerly "Reset appearance to defaults") resets font, colors,
opacity and window size, and keeps the theme.

When the desktop asks for high contrast, the theme palette is not applied
and libadwaita's high-contrast style is used.

## Palette

Colors are defined once, in `crates/lcrt-ui-gtk/src/theme.rs`.

| Role | Pop Studio | Calm Native | Cinema Focus |
| --- | --- | --- | --- |
| Background | `#2A1E5C` | `#F6F7FA` | `#141721` |
| Surface (lists, dialogs, control bar) | `#39276B` | `#FFFFFF` | `#212637` |
| Field (menus, popovers) | `#4C387C` | `#FFFFFF` | `#2A2E43` |
| Text | `#FFFFFF` | `#272A3D` | `#FAFAFF` |
| Primary action / text on it | `#FF5C8A` / `#2A1E5C` | `#2A1E5C` / `#FFFFFF` | `#FFD23F` / `#2A1E5C` |
| Highlight (focus rings, links) | `#FFD23F` | `#2A1E5C` | `#FFD23F` |
| Accent / text on it (preview, themed badges and outline) | `#FFD23F` / `#2A1E5C` | `#FF5C8A` / `#2A1E5C` | `#2A1E5C` / `#FFFFFF` |
| Header bar / text on it | `#FFD23F` / `#2A1E5C` | `#FFFFFF` / `#272A3D` | `#1A1E2B` / `#FAFAFF` |

## Accessibility changes from the reference palettes

Unit tests check every pair above against WCAG 2 contrast
(`theme::tests::every_theme_s_text_reads_at_wcag_aa_on_every_surface`).

- **Muted text.** The reference Calm Native muted color `#6E7388` is 4.38:1
  on `#F6F7FA`, below AA. LILOPOP draws muted text as dimmed text instead,
  and dims to 70% rather than libadwaita's 55% (55% gives 3.38:1 on Calm
  Native and falls short on Pop Studio menus). At 70%, muted text is at
  least 4.5:1 on every surface of every theme. If the design keeps a
  separate muted color, `#6A6F84` is the closest AA-passing value
  (4.65:1 on the background).
- **Stop button.** libadwaita draws destructive buttons as a 15% tint,
  which is 4.42:1 on Pop Studio's control bar. The control bar's Stop is a
  solid red button with white text instead (6.1:1 dark, 4.8:1 light).
- **Focus rings.** libadwaita derives the ring from the action color. On
  the dark themes that is faint, so rings use the highlight color: 3.06 to
  3.94:1 on every background and surface. On Pop Studio menus the ring is
  2.84:1, but menu items show focus with a fill as well.
- **Caption readability exception.** No theme recolors caption text or its
  background. Pink or yellow caption text is hard to read over real video,
  so captions keep the neutral, user-chosen caption style in every theme.

## Not covered yet

These belong to the future full UI redesign (Blueprint v0.2), not to the
theme foundation:

- The Blueprint's twelve surfaces (control center layout, onboarding,
  explicit Preparing / Ready / Connecting / No speech states with their own
  copy, overlay placement controls) are not implemented. Themes style the
  states that exist today: Ready, Starting, Listening / Translating,
  Stopping, Stopped and errors.
- The Pop Studio yellow app bar from the visual direction applies to raised
  header bars only. libadwaita's Settings window uses a flat header, which
  shows the window background.
- The caption window cannot be narrower than its control bar (about
  670 px at 100% scale, wider with translation lanes), whatever width is
  set in Caption style. This predates themes.
