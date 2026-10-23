# Host connections

AGS can expose selected **host Unix stream sockets or loopback TCP services** to a
sandbox. This is independent of SSH, GitHub, MCP, and any particular host OS.
Client configuration and credentials remain separate from transport.

## Configure endpoints

Declare connections only in the trusted base config (`~/.config/ags/config.toml`
or an explicitly selected `--config` file):

```toml
[[host_connection]]
name = "git-auth"
host_unix = "/run/my-key-service/auth.sock"
sandbox_unix = "/run/ags-connections/git-auth.sock"

[[host_connection]]
name = "github-api"
host_unix = "/run/my-api-broker/api.sock"
sandbox_unix = "/run/ags-connections/github-api.sock"

[[host_connection]]
name = "local-mcp"
host_tcp = "127.0.0.1:4788"
sandbox_tcp = "127.0.0.1:4788"
```

Each entry has exactly one `host_unix` or `host_tcp`, and exactly one
`sandbox_unix` or `sandbox_tcp`. The transports can be mixed in either direction.

- Names are unique, 1–64 ASCII letters, digits, `_` or `-`.
- `host_unix` is an absolute path (no `~`/environment expansion).
- `sandbox_unix` must be `/run/ags-connections/NAME.sock`. AGS owns this directory;
  other mounts must not overlap it. The filename need not equal the connection name.
- TCP addresses are **numeric loopback addresses** with nonzero ports. IPv4 and
  IPv6 are supported; DNS names, wildcard binds and non-loopback addresses are rejected.
- Sandbox endpoints must be distinct, including disabled declarations.
- At most 32 connections may be declared. Unknown fields are errors.
- `enabled = true` is the default. Every enabled connection is required.
- `allow_hooks = false` is the default.

A fixed sandbox loopback port is isolated by each container's network namespace.
Different AGS sessions can all use port 4788 without taking a host listening port.
`host.containers.internal` is not needed, and no host service must bind to the LAN.

## Lifecycle and access

AGS creates a private per-session directory and a host-side Unix listener for each
selected endpoint. The directory is mounted read-only into the sandbox; the actual
upstream sockets and credential directories are **not mounted**. The relay connects
as the invoking host user, with that user's existing group access. The container
needs neither those supplementary groups nor host networking.

Before launch AGS connects to and closes each selected host endpoint. A failed probe
aborts the launch. `ags doctor` also performs these probes for enabled connections.
**These probes send no bytes but can appear as accepted/closed connections in service
logs. They prove reachability, not authentication or protocol compatibility.** Disabled
connections are reported without connecting. Hook execution is not part of doctor.

For sandbox TCP endpoints, an embedded Python 3 supervisor binds all loopback
listeners before starting the existing AGS entrypoint. A bind conflict or missing
interpreter fails the launch; it does not start the agent without the forward.
Python 3 is in the standard AGS image; custom images need `/usr/bin/python3`.
Unix-only connections need no container-side helper.

The host opens a fresh upstream connection for every client, so socket replacement
or host-service restart does not pin subsequent clients to an old socket inode.
Upstream failures close affected streams; there is no token fallback or replay of
partially delivered requests. Both relays bound concurrent clients to 32 per endpoint
and use bounded buffers with backpressure. Half-close is preserved.

Relays live while the attached AGS invocation is running. Normal exit and handled
launch errors close active streams and remove private runtime files. They are not
persistent services for independently detached containers. If AGS crashes/is killed,
its relay connections close; private runtime files may remain until runtime-directory
cleanup. Restart AGS rather than trying to reconnect a surviving container to a dead
relay. Remote Podman is refused. `--lockdown` omits configured connections and rejects
hook requests for them before starting sidecars.

## Prepare hooks request names, not endpoints

For a connection to be selected only when a hook requests it:

```toml
[[host_connection]]
name = "local-mcp"
host_tcp = "127.0.0.1:4788"
sandbox_tcp = "127.0.0.1:4788"
enabled = false
allow_hooks = true
```

A prepare hook can return:

```json
{"version":1,"connections":["local-mcp"],"env":{"SERVICE_TOKEN":"..."}}
```

`connections` is an optional additive field in the v1 output protocol. Older AGS
versions reject the unknown field; responses without it remain compatible. Duplicate
requests are deduplicated. Unknown or ungranted names fail closed, even if the named
connection was already enabled. Hooks cannot turn connections off or invent host
endpoints. Repository overlays cannot declare or replace `[[host_connection]]`.
An explicitly selected base config is trusted host configuration: do not select an
unreviewed repository file as `--config`.

`ags hooks test NAME` validates activation grants and prints names, not environment
values. It does not start relays or probe endpoints. See [startup hooks](STARTUP_HOOKS.md)
for executable approval and execution authority. Hooks run as host code; these
restrictions on their output do **not** sandbox the hook itself.

Literal hook environment values use the private env-file transport, not Podman's
command-line arguments. They are still available to sandbox processes and to the
container engine. No secret-protection property should be inferred from forwarding.

## Client recipes

The transport intentionally does not rewrite client configuration, mount private
keys, fetch tokens, or replace the host's harness launcher.

### SSH and Git signing

Connect the appropriate client to `/run/ags-connections/git-auth.sock` with OpenSSH's
`IdentityAgent` setting. Keep pinned GitHub host keys, `IdentitiesOnly`, and the
appropriate public identity in a reviewed SSH config. For signing, declare a second
connection to the signing agent and use a container-native signing wrapper that sets
`SSH_AUTH_SOCK` to its forwarded socket before invoking `ssh-keygen`.

Public metadata can be staged/mounted separately with existing config mechanisms.
Host Nix wrappers commonly reference `/nix/store` paths unavailable in the Fedora
container: use the container's OpenSSH binaries, not those host wrappers.

AGS's existing dedicated SSH agent remains unchanged. These connections do not
implicitly switch Git to the host identity. Supply a reviewed existing
`[sandbox].gitconfig_path` pointing to the container signing wrapper and public key;
AGS does not overwrite an existing Git config. Do not mistake AGS-generated keys for
registered host credentials.

### GitHub API broker

Configure the container's `gh` with a dedicated `config.yml`:

```yaml
version: "1"
http_unix_socket: /run/ags-connections/github-api.sock
git_protocol: ssh
```

Include the schema version before mounting this configuration read-only: current
`gh` otherwise attempts a config migration and fails on the read-only filesystem.

A container-native wrapper should select that config and the broker's documented
placeholder token, clearing other GitHub token variables. The host broker retains
the real PAT. The generic transport neither knows nor invents the placeholder's value.
Do not use `gh auth token` as a source for a real sandbox token in this setup.

### Local HTTP MCP and a host credential wrapper

Keep MCP registration pointed at `http://127.0.0.1:4788/mcp` and declare a matching
loopback forward. Secret injection remains a separate prepare hook or `[[secret]]`
source. With agent-box, its existing `agent-box-executor run ABSOLUTE_PROGRAM ...`
action can launch an environment-emitter program after validating and setting
`AGENT_BOX_EXECUTOR_KEY`. Thus no new token-export API is necessary: a reviewed host
hook can call that wrapper and emit the v1 JSON environment contribution.

The emitter must consume the hook context, fail if the key is absent, and serialize
only the intended variable to JSON stdout. Never log the key or copy the client-key
file into the container. Hooks start with a scrubbed environment; simply reading the
parent shell's variable does not work. Keep the wrapper and hook on the host, where
its ownership/group checks and Nix interpreter paths are valid.

Caution: existing hook rules allow **literal environment contributions in lockdown**,
including values a hook fetched itself. Do not globally enable a credential-loading
hook for lockdown sessions assuming AGS can recognize the literal as a secret. A hook
that also requests its required connection is refused in lockdown before launch,
although approved host hook code has already executed.

### Model gateways are separate from MCP

A working MCP connection does **not** give Pi, Claude, or Codex access to models.
Host harness launchers may supply a model gateway URL, its client credential, and
provider-specific flags. AGS launches container-native harnesses, not those host
launchers, so their environment must be configured separately.

For example, a model gateway on host loopback port 8317 needs its own connection,
even if Executor MCP on port 4788 already works:

```toml
[[host_connection]]
name = "model-gateway"
host_tcp = "127.0.0.1:8317"
sandbox_tcp = "127.0.0.1:8317"
enabled = false
allow_hooks = true
```

An approved prepare hook can obtain the gateway **client** credential from a
validated host source and return `"connections": ["model-gateway"]` in its JSON
response, together with the required environment variables. Keep upstream provider
OAuth credentials on the host. Scope the contribution to the agents that use it;
a connection request also prevents silently injecting the credential in lockdown.

Client wiring still matters:

- **Pi:** its mounted `models.json` needs the gateway's compatible API and base URL
  (for example `http://127.0.0.1:8317/v1`), plus a reference to the injected client-key
  environment variable. Mounted model definitions alone do not supply credentials.
- **Claude:** supply `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` for its gateway.
  When supported by the installed Claude version, enable
  `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` to discover gateway models. Reserved
  `CLAUDE_*` variables cannot be supplied by prepare hooks: put those non-secret
  client settings in reviewed trusted-base configuration, such as a read-only
  `/etc/profile.d` script. Clear conflicting direct-provider authentication/routing
  when selecting this gateway.
- Do not inject Claude's Anthropic credential into Pi just because both use the
  same gateway: Pi may interpret it as credentials for direct Anthropic models.

Verify more than `ags doctor`: connection probes only prove that a socket accepts
connections. Check the model list **inside the sandbox**, then request a small,
no-tool response through each harness, including its default model selection.
A model advertised by a gateway may still be rejected by the upstream account;
that is distinct from a missing forward or client credential.

## Security limits

These are byte-stream relays, not protocol authorization proxies. A granted endpoint
confers whatever authority its upstream service allows. Docker/Podman sockets, broad
SSH agents, and authenticated management endpoints can confer substantial host power.
Unix services see the **host relay user's credentials**, not the container client's
credentials. Validate that this matches their authorization model.

No datagrams, file-descriptor passing (`SCM_RIGHTS`), or preservation of per-client Unix
peer credentials is supported. This does not make arbitrary Wayland/desktop sockets
forwardable. Same-host processes with the invoking UID are not isolated from the
relay; read-only mounts prevent filesystem changes, not protocol writes. Tokens used
by HTTP services should still be authenticated upstream.
