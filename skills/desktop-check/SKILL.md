---
name: desktop-check
description: "Inspect and verify behavior in a Windows application window."
---

# Desktop check

1. Use computer_list to find the target by application and title. Ask the user for the intended application only if several targets remain plausible.
2. Observe the window before requesting input. Use the returned observation ID and element index or snapshot coordinates. Treat a stale observation or focus change as a reason to observe again.
3. Report only checked UI state and steps.
