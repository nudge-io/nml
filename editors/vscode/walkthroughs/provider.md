# Use your tool's schema

If your project is built with a schema-provider tool (for example **nudge**),
declare it once in `nml-project.nml` at your project root:

```
project MyApp:
    provider:
        tool = "nudge"
```

What this does:

- In a **trusted** workspace, the editor launches the tool's own language
  server (`nudge lsp`) and validates against the **exact tool binary** you have
  installed — no sync step, always in step with what deploys.
- In an **untrusted** workspace, or when the tool is not on your `PATH`, the
  neutral server serves the tool's **published package** by the same name.

Launching a tool is gated: you are asked once per workspace, and a binary that
lives *inside* the workspace is never launched.
