# Telemetry

siGit Code itself collects no usage analytics. On-device models run on
[Onde Inference](https://ondeinference.com), and Onde can report model timing to its own
dashboard. That reporting only switches on in builds where Onde's credentials were embedded at
compile time. When it is on, siGit Code sends:

- one event when an on-device model finishes loading: the model's Hugging Face repo ID, its
  display name, and how long the load took;
- one event per on-device response: the model ID, a per-process request ID, the response time,
  the time to first token when it was measured, and whether it succeeded, errored, or was
  cancelled.

Nothing from your conversation goes with them: no prompts, responses, file paths, or code.
siGit Code doesn't set a machine identifier, so every install reports as the same
`onde-unknown` edge, and it sends no location. Models you run through siGit Code Cloud or your
own endpoint send no Onde events.

To see whether your build reports, load an on-device model and check the log for
`ChatEngine: pulse telemetry enabled` or `ChatEngine: pulse telemetry disabled`. siGit Code logs
to stderr, which an editor shows in its agent log. Terminal mode writes it to
`$TMPDIR/sigit.log`.

To turn it off, set `ONDE_DISABLE_PULSE=1` in the environment siGit Code runs in. For an
editor, put it in the agent's `env` block:

```json
"env": { "ONDE_DISABLE_PULSE": "1" }
```
