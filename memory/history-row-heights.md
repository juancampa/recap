---
name: history-row-heights
description: The History panel row heights are a per-frame cache; scroll math must refresh it after a refilter
metadata:
  type: project
---

`App::hist_row_heights` holds the visual height of each History display row.
The main loop calls `recompute_row_heights` once per frame, before the draw.
Any code that changes `display_rows` between two frames (`refilter_preserving`,
`rebuild_display`) leaves that cache stale, and `row_height` then returns 1 for
every row past the end of the old vector.

**Why:** `adjust_scroll` sums `row_height` to place the window. With stale
heights it stops too early and leaves the selected row off the screen. The mode
switch (`m`) showed this: the row needed 75 lines of a 20-line window.

**How to apply:** `adjust_scroll` calls `refresh_row_heights`, which recomputes
at `last_list_width` (the width of the last frame). Call `refresh_row_heights`
first in any new function that measures rows.
