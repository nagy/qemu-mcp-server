# MCP server plan — hardening and future work

Audit-driven plan for `qemu-mcp-server`: what was hardened now, what was
deliberately not adopted (with reasons), and what comes later. The
checklist source is the mcp-builder skill (anthropics/skills:
`SKILL.md` + `reference/mcp_best_practices.md`).

## Applied now (mcp-audit branch)

### Tool naming: `qemu_` prefix

Skill rule: `{service}_{action}` snake_case names, because an agent
session often runs several MCP servers side by side and generic names
collide or confuse the model about tool ownership. So:
`qemu_execute_qmp`, `qemu_read_serial`, `qemu_write_serial`. Renaming
is cheap while no agent configs reference the old names.

### Tool annotations

Every tool declares `readOnlyHint` / `destructiveHint` /
`idempotentHint` / `openWorldHint` via rmcp:

- `qemu_execute_qmp` — a passthrough, so the schema cannot say which
  commands mutate the VM. `read_only=false`, `destructive=false`
  (additive in the annotation sense; the description names the
  state-changing commands explicitly), `idempotent=false`,
  `open_world=false` (closed world: one local QMP socket).
- `qemu_read_serial` — `read_only=true` (does not touch the guest) but
  `idempotent=false`: each call consumes buffered output, and
  `wait_for` blocks.
- `qemu_write_serial` — `read_only=false`, `destructive=false`
  (additive: bytes are sent), `idempotent=false` (sending the same
  data twice sends it to the guest twice), `open_world=false`.

### QMP round-trip timeout

`run_command` now wraps the whole connect/negotiate/execute round trip
in a 30 s timeout (`DEFAULT_QMP_TIMEOUT_MS`). A wedged QEMU — paused
process, overloaded VM, stuck monitor — surfaces a tool error instead
of hanging the agent's request forever. `qemu_read_serial` already had
its own `timeout_ms`.

### Actionable error strings

Errors carry next-step hints at the failure site, not only in the
server instructions:

- QMP/serial connect failures append the exact QEMU flags to fix it
  (`-qmp unix:<path>,server,nowait`, `-serial unix:<path>,server,nowait`).
- The QMP timeout error names likely causes.
- The serial timeout error already told the agent to re-read the raw
  buffer without `wait_for`.

## Deliberately not adopted, with reasons

- **HTTP transport + auth (Origin guard, bearer token).** The stdio
  model fits this server: the agent spawns it, the server is a thin
  client of user-owned sockets, there is no listener to attack.
  Revisit only if a persistent-server mode lands (see below).
- **`outputSchema` for `qemu_execute_qmp`.** The result is arbitrary
  QMP JSON — no schema to declare. Declaring a loose
  `{"type": "object"}` adds noise without helping clients.
- **Pagination / JSON+Markdown dual response formats.** No listing
  tools; not a data-retrieval server.
- **`{service}-mcp-server` server naming convention.** The package is
  already named that way; the MCP server name is `qemu-mcp-server`.

## Later (not started)

- `qemu_get_status` — wraps `query-status` plus connection state
  (QMP socket reachable, serial connected, buffer size). Typed
  `outputSchema`. Gives the agent a cheap "am I talking to the right
  VM" check and works when QMP hangs (timeout-bounded).
- `qemu_screenshot` — QMP `screendump` (`format: png` on newer QEMU),
  returned as base64 for direct re-use by image-display MCP servers,
  so an agent can see the VM's display without VNC.
- Optional persistent-server mode: listen on loopback (Streamable
  HTTP) so the serial buffer survives across agent sessions and
  several agents can share one VM view. If it lands: bind loopback
  only, reject any request carrying an `Origin` header (browsers
  always send one on cross-site requests; raw MCP clients do not —
  this closes the DNS-rebinding/drive-by hole), optional per-run
  bearer token in `${XDG_RUNTIME_DIR}` (0600).
- Evaluation pass (skill phase 4): ~10 verifiable agent tasks, e.g.
  "boot a kernel image, wait for the login prompt via
  `qemu_read_serial`, log in, report `uname -a` output".

## Testing

- Unit: QMP request wire format, event skipping, error mapping
  (CommandNotFound → METHOD_NOT_FOUND), greeting validation,
  serial buffer/watermark behavior, serial timeout, and the new QMP
  round-trip timeout (short-duration variant against a silent fake
  QEMU).
- Manual: two-terminal smoke test with a real QEMU (`-qmp unix:...`
  plus `-serial unix:...`), driving a guest through
  `qemu_write_serial` / `qemu_read_serial`.
