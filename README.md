# cue-rust

A Windows desktop helper that takes a screenshot, reads the coding problem on screen, and returns a solution in a small window that stays out of the way.

## Capture limits

The window asks Windows to leave it out of screenshots and screen shares. That covers normal capture, including the screenshot the app takes for itself.

If a proctoring app or remote desktop grabs the screen at the hardware level, Windows cannot hide the window from it. In that case, hide the window while capture is active, or run the helper on a different machine.
