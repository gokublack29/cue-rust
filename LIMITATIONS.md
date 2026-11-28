# Limitations

This file describes what the app cannot do, and what can still go wrong.

## Capture and stealth

Windows can hide the Cue window from normal screenshots and screen shares. The app uses the Windows setting that excludes a window from capture.

That setting does not stop every kind of capture. A proctoring app or remote desktop that reads the screen at the hardware level will still see the window. Nothing in this app can change that. If that matters, hide Cue while capture is active, or run it on another machine.

Hiding the window does not hide the process. A platform can still detect a separate program running.

The app asks Windows to keep Cue out of the taskbar and Alt+Tab. Windows can restore those flags when the window changes, so the app sets them again. On some Windows versions the window may still appear in other system lists, such as Task Manager.

Only one copy runs at a time. Starting a second copy brings the first window to the front and exits.

## Platform

The app is built for Windows. It still compiles on other systems, but screen capture, hotkeys, stealth, and window control do nothing there. Screen capture returns an error.

The exe is built locally and is not signed. Windows may show a SmartScreen warning on first run.

## Keys and network

Groq API keys are stored in plain text in `groq_keys.txt` or `.env`. Anyone with access to the folder can read them.

Keys are loaded from the folder next to the exe first, then the folder the app starts in.

The app needs a network connection to Groq. Every captured screenshot is sent to Groq for reading. If that is a problem for the data on screen, do not use the app with that data.

The model names are fixed in the code: `qwen/qwen3.8-27b` for reading the screen and `openai/gpt-oss-120b` for solving. If Groq changes or removes those models, or the key cannot use them, requests fail.

Rate limits are handled by rotating keys. When every key is limited, the app waits for the soonest reset (up to 60 seconds), shows a countdown, and retries once. If it is still limited, the request stops. Preemptive rotation depends on a response header. If that header is missing, the app cannot rotate early.

## Screen reading

Capture downscales long edges above 1920 pixels. On 4K or ultrawide screens, small text can blur and the reading can get worse.

The app picks the monitor of the active non-Cue window. If Cue itself is focused, it uses the monitor under the mouse. Unusual window or monitor setups can pick the wrong screen.

The solver writes in the language it sees in the editor. If no language is visible, it falls back to Python 3. It can still pick the wrong language or function signature.

The two-stage pipeline expects Groq's stream format. A change in that format can break streaming or status messages.

## Testing

There are 21 tests. Twenty run by default. The skipped one needs real API keys and an image.

No automated test checks the real window, hotkeys, or stealth flags. Those were checked by running the app on Windows.
