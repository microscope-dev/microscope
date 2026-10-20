# Upload recordings

`microscope agent` keeps a machine connected to a microscope service, which
stores recordings and gives each one a link. The agent uploads the
recordings you queue, keeps each until the service confirms it, and resumes
an upload a dropped connection interrupted. With your permission, it also
records processes that you choose in the service's pages.

The agent is for developers' own workstations. It needs no privilege beyond
what [`microscope record --pid`](profiling.md#production-permissions) needs.

## Sign in

Make a device token for the machine in the service's pages, under Device
tokens. The service shows it once. Save it with the service's URL:

```sh
microscope agent login --server https://service.example/
```

The command reads the token from standard input, so it can come from a
pipe. It writes the token to `~/.config/microscope/agent-token`, which only
your user can read, and the URL to `~/.config/microscope/agent.toml`.

## Run the agent

```sh
microscope agent
```

It prints `connected to URL as EMAIL` once the service accepts the token,
and reconnects by itself after a network failure or a service restart,
waiting from one second up to a minute between attempts. It stops with an
error if the service refuses the token, as it does once the token is
revoked. Ctrl-C stops it.

## Upload a recording

Add `--upload` to `microscope record`:

```console
$ microscope record --upload --duration 2 -o spin.usprof -- ./spin.sh
microscope: recorded 1905 samples to spin.usprof
microscope: the kernel dropped 79 samples
microscope: uploading spin.usprof; a recording holds the program's paths, command lines, and function names, and stays private to your account until you share it
http://localhost:8080/recordings/c44ef84733204338dcd02a00bb89b650
```

The link goes to standard output. If the agent has not uploaded the
recording within 30 seconds, as when it is not running, the recording waits
in the outbox, `~/.local/state/microscope/outbox`, until the agent next
connects.

Only you can open a recording until you share it from its page.

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
command.

## Settings

`~/.config/microscope/agent.toml` holds the agent's settings. They are kept
apart from `config.toml`, whose project files a repository can ship, so a
project cannot choose where your recordings go.

| Setting | Meaning |
|---|---|
| `server` | The service's URL; `agent login --server` sets it |
| `allow-remote` | Run the commands the service sends, as `--allow-remote` does |
| `watch` | Directories of continuous segments to upload, as `--watch` does |

```toml
server = "https://service.example/"
allow-remote = true
watch = ["/home/me/profiles"]
```

## When an upload fails

The service refuses a recording for good when it is larger than the service
accepts or the account is out of room. The agent then keeps it in the outbox
as `KEY.usprof.failed`, with the reason in `KEY.error`, and goes on with the
rest. When the service fails for a moment, the agent tries the recording
again a minute later.

> TODO: document running the agent as a systemd user service after a unit
> file ships with microscope.
