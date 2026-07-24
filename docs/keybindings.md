# Plux Keybindings

The default prefix is `Ctrl-A`. Configure it with `prefix = "Ctrl-]"` or
another supported control key.

Use `plux <name>` to enter a session. Plux creates a missing session and
automatically replaces an older attached client. Running `plux` without a name
uses the `default` session. For a remote server, use
`plux --ssh user@server <name>`.
Use `plux stop` to end the daemon and all of its panes.

## Normal Mode

`PageUp`, `PageDown`, and the mouse wheel scroll the focused pane directly and
enter scroll mode.

When the focused application uses the alternate screen, such as Codex, Vim, or
Less, `PageUp`, `PageDown`, and the mouse wheel are forwarded as application
pagination. Plux's scrollback applies to the normal shell screen.

When the focused application does not capture the mouse, left-button dragging
selects text and copies it on release. In scroll mode, Plux always owns mouse
selection.

Use the terminal's normal keyboard paste shortcut. When mouse capture is active,
use Shift with the terminal's right-click paste action to bypass mouse reporting.

| Keys | Action |
|---|---|
| Prefix then `d` | Detach |
| Prefix then `c` | Create a vertical pane |
| Prefix then `v` | Create a vertical split |
| Prefix then `s` | Create a horizontal split |
| Prefix then `h/j/k/l` | Focus left/down/up/right |
| Prefix then `+`/`>` | Increase the focused split ratio |
| Prefix then `-`/`<` | Decrease the focused split ratio |
| Prefix then `z` | Toggle zoom |
| Prefix then `x` | Close the focused pane |
| Prefix then `r` | Rename the attached session |
| Prefix then `[` | Enter scroll mode |
| Prefix then `/` | Search history |

## Scroll Mode

| Keys | Action |
|---|---|
| `j` / `k` | Scroll down/up |
| PageUp / PageDown | Scroll by page |
| `g` / `G` | History top/bottom |
| `/` | Search history |
| `n` / `N` | Repeat search forward/backward |
| Left-button drag | Select text and copy on release |
| `v` | Start or finish coordinate selection |
| `h/j/k/l` | Move selection cursor |
| `y` | Copy selection, or the visible pane without a selection |
| `q` / Escape | Leave scroll mode |
