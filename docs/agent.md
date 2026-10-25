# Upload recordings

`microscope agent` keeps a machine connected to a microscope service, which
stores recordings and gives each one a link. The agent uploads the
recordings you queue, keeps each until the service confirms it, and resumes
an upload a dropped connection interrupted. With your permission, it also
records processes that you choose in the service's pages.

The agent runs on workstations and managed hosts. It needs no privilege beyond
what [`microscope record --pid`](profiling.md#production-permissions) needs.

## Configure a project key

Open Machines, choose Set up agent, and select the destination project.
A project administrator creates a named agent key, shown only once. A key
can serve one machine or a deployment cohort. Save it with the service's URL:

```sh
microscope agent login --server https://service.example/
```

Paste the key at the prompt, or supply it through standard input. Do not put
it in a command argument or shell history. The command writes the key to
`~/.config/microscope/agent-token`, readable only by your user, and the URL
to `~/.config/microscope/agent.toml`. Starting the agent verifies the key
with the service. HTTPS is required except on localhost and loopback addresses.

The key permits agent connections, uploads, and replies to service commands.
It grants no permission to read stored recordings, administer the project,
or command another agent. Organization project keys belong to the project,
not to the person who created them.

For fleet provisioning, point at a private file from your secret manager:

```sh
microscope agent login --server https://service.example/ --key-file /run/secrets/microscope-key
```

The file must be readable only by the agent's OS user (`chmod 600`). This
stores its path, not a copy of the secret. You can also pass `--key-file`
when starting the agent.

## Run the agent

```sh
microscope agent
```

It prints `connected to URL for project OWNER/PROJECT` once the service accepts the key,
and reconnects by itself after a network failure or a service restart,
waiting from one second up to a minute between attempts. It stops with an
error if the service refuses the key, as it does once the key is disabled.
Ctrl-C stops it. Only one uploading agent may use an outbox at a time.

## Replace or disable a key

For planned replacement, create another key in the same project, distribute
it, and verify uploads before disabling the old key. Replacing the private
key file atomically makes the running agent reconnect with the replacement;
its recordings stay queued and interrupted uploads resume. `agent login`
also replaces the default key file atomically.

For a compromised key, disable it immediately in Agent setup. Agents still
using it stop and cannot reconnect; replace their key and start them again.
Disabling a key does not delete recordings already stored. Recent key use
helps track deployment but does not prove an offline machine was updated.

An outbox is bound to the service and project from its first successful
connection. A replacement key for another destination is rejected before
uploading. Use separate `XDG_CONFIG_HOME` and `XDG_STATE_HOME` directories
for agents submitting to different projects.

## Upload a recording

Add `--upload` to `microscope record`:

```console
$ microscope record --upload --duration 2 -o spin.usprof -- ./spin.sh
microscope: recorded 1905 samples to spin.usprof
microscope: the kernel dropped 79 samples
microscope: uploading spin.usprof; a recording holds the program's paths, command lines, and function names, and is accessible to people with project read access
http://localhost:8080/recordings/c44ef84733204338dcd02a00bb89b650
```

The link goes to standard output. If the agent has not uploaded the
recording within 30 seconds, as when it is not running, the recording waits
in the outbox, `~/.local/state/microscope/outbox`, until the agent next
connects.

People with read access to the destination project can open its recordings.

## Upload a continuous recording

`--watch DIR` uploads each segment that
[`record --continuous DIR`](profiling.md#keep-a-rolling-window) finishes.
The segments stay in `DIR`, and each is uploaded once, across restarts:

```sh
microscope record --continuous profiles --pid 1842 &
microscope agent --watch profiles
```

## Record from the service's pages

The service can list a machine's processes and record one for a number of
seconds, but only when you allow it:

```sh
microscope agent --allow-remote
```

The agent then shows its user's processes, never another user's, and records
one when asked, the way `microscope record --pid` does. The recording is
uploaded like any other. Without `--allow-remote`, the agent refuses every
command. The service separately checks the requesting person's project
permission. Sharing an agent key does not grant that permission.

## Settings

`~/.config/microscope/agent.toml` holds the agent's settings. They are kept
apart from `config.toml`, whose project files a repository can ship, so a
project cannot choose where your recordings go.

| Setting | Meaning |
|---|---|
| `server` | The service's URL; `agent login --server` sets it |
| `key-file` | Private project key file; otherwise use `agent-token` beside this file |
| `allow-remote` | Run the commands the service sends, as `--allow-remote` does |
| `watch` | Directories of continuous segments to upload, as `--watch` does |

```toml
server = "https://service.example/"
allow-remote = true
watch = ["/home/me/profiles"]
```

## When an upload fails

The service refuses a recording for good when it is larger than the service
accepts or the project owner is out of room. The agent then keeps it in the outbox
as `KEY.usprof.failed`, with the reason in `KEY.error`, and goes on with the
rest. When the service fails for a moment, the agent tries the recording
again a minute later.

> TODO: document running the agent as a systemd user service after a unit
> file ships with microscope.
