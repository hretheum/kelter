# Field Notes: Deterministic Agent Kernels

Agent orchestration keeps failing in the same place: the model is the
runtime. This note collects the argument for inverting that.

## Why weak engines break orchestrators

When tool calls are the control flow, every malformed call is a crashed
workflow. Reliability becomes a property of the model, not the system.
See [the MCP spec](https://modelcontextprotocol.io) for the tool substrate
side of the story.

## What a kernel changes

A deterministic state machine owns retries, persistence and error handling.
The model only proposes actions from a schema — see
[constrained decoding](https://en.wikipedia.org/wiki/Context-free_grammar)
for how a grammar makes malformed proposals unrepresentable.

## Open questions

Latitude tuning per engine capability, and memory retrieval quality metrics,
both deserve their own notes. Related reading:
[Quadlet](https://docs.podman.io/en/latest/markdown/podman-systemd.unit.5.html).
