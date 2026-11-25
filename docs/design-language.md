# Phase

Buddy takes its shape from a 3½-inch floppy disk. Its clipped corner, metal shutter and ruled label carry through the app. Three parallel orange lines jog together across the label. They also appear beside the wordmark and fill as a transfer advances. [Interactive reference](previews/phase.html)

## Palette and type

| Role | Color | Use |
| --- | --- | --- |
| Canvas | `#101011` | Window background |
| Surface | `#19191A` | Cards, dialogs and grouped controls |
| Inset | `#141415` | Search and editable fields |
| Hairline | `#343436` | One-point dividers and quiet borders |
| Text | `#F2EEE7` | Titles, values and control labels |
| Secondary text | `#AAA8A5` | Paths, counts and supporting details |
| Signal orange | `#FF681E` | Primary action, focus, selection and transfer progress |
| Text on orange | `#17100C` | Labels inside filled orange controls |

Use the wide, low Buddy wordmark for the brand. Use a legible proportional sans for game titles, forms and actions. Set paths, version IDs, timestamps and byte counts in monospace. Preserve the casing of game names and paths.

## Screens and states

- **Library.** Put search and filters before the games. Use wide tiles with the game title and one short status. A locally cached Steam hero can fill a tile. Darken the text area so bright artwork stays readable. When art is missing, corrupt or unavailable, draw a floppy cover from the stable game ID. Its label stays recognizable after a search, a sort or a restart. Artwork belongs to the presentation layer and can arrive after the library is usable.
- **Files.** Keep the game identity visible while choosing locations. Put the selected paths, included files and total size close to the save action. Expose platform overrides and infrequent settings on demand.
- **Conflicts.** Give the local and server copies equal visual weight. Show device, time and file information together. Distinguish the selected version with a visible mark as well as an outline. Keep the permanent-backup status and the option to defer visible before the player commits.
- **Transfers.** Name the current operation. Fill the three traces from actual byte counts when a total exists. Use an indeterminate state while checking or preparing files. Short successful syncs can finish quietly. [Sync and recovery](sync.md)
- **Forms and dialogs.** Group fields by task and give the main action one orange fill. Keep addresses, fingerprints, offline choices and destructive consequences legible at the point of decision.

## Layout and interaction

Let the task set the hierarchy. Library search and filters stay visible. Put occasional library operations in a **…** menu beside the library title. Lead a game overview with the current save and **Sync now**. Keep file paths and Steam setup expandable. In backup lists, show the source device and time alongside **What changed?** and group changes to a revision in a trailing **…** menu.

Give each overflow button a contextual tooltip and keyboard focus. Use quiet, full-size controls for back, cancel and dismissal. Show their outline on hover or keyboard focus. Keep recovery choices, trust checks and confirmation consequences visible at the decision.

Use flat charcoal surfaces, thin borders and softly squared corners. Group related controls with roughly 8 points of space, and leave 16–24 points between sections. Keep orange concentrated around the next action or the current selection. Pair important states with text or a recognizable mark so color alone never carries the decision.

Let rows wrap and comparison panels stack when space is tight. Keep values selectable where players may need to copy them. Focus must remain visible with a keyboard. Give controls short verb labels such as **Save setup** and **Decide later**. Add explanatory copy when it helps someone judge a consequence, identify a save, or recover from a failure.
