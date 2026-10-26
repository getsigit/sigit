# Terminal mode

Run `sigit` in a terminal and you get the same model and system prompt as the editor integration, just in a full-screen chat UI.

Terminal mode currently needs Unix terminal behavior, so it works on macOS and Linux only.

The interactive UI emits [OSC 7501 program status](https://www.superlogical.com/rex/docs/build/program-status) reports so supporting terminals can show working, waiting for permission or input, browser sign-in, completion, and errors across tabs. User-driven quit reports idle; terminals expire active work on process exit and retain completion or error reports. Status sequences are never written to headless/JSON or ACP output.
