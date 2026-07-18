# ADR 0002: MCP as the only tool/plugin substrate

Status: accepted · Date: 2026-07-18

## Context

A custom plugin API would need its own SDKs, docs, and ecosystem. Meanwhile
MCP (Model Context Protocol) is an open standard with a growing tool
ecosystem, official SDKs, and native support in major agent clients.

## Decision

The kernel has **zero custom plugin API**. Every tool or integration is an
MCP server; the `mcp` crate is an MCP *client* that spawns configured servers
(stdio child processes), lists their tools, converts tool JSON Schemas into
the kernel's action space, and executes validated actions as MCP tool calls.
The reference tool set for the demo slice ships as a bundled MCP server
binary (`tools-server`) — it goes through exactly the same protocol path as
any third-party server.

## Consequences

- Ecosystem interop for free: any existing MCP server is a plugin.
- Tool schemas arrive as JSON Schema, which is exactly what the kernel
  validates proposals against — one schema language end to end.
- The kernel↔executor seam speaks the same protocol as external agent
  tooling that talks MCP natively, removing a whole class of glue.
- Protocol overhead (spawning servers, JSON-RPC) is accepted even for
  built-in tools to keep one honest path.
