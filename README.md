<img src="assets/logo.svg" alt="omp-deck logo" width="64" height="64" align="right">

# omp-deck

A small CLI that serves a web dashboard of the live
[`omp`](https://github.com/can1357/oh-my-pi) collab sessions on this machine,
so you can open any of them from your phone.

One card per session: name (hover/long-press for the full working directory),
model, a busy / input-required / idle badge, participants and how long ago it
started, with **view** and **control** links that open the `my.omp.sh` room.

## Install

```sh
cargo install omp-deck
# or from a checkout
cargo install --path .
```

`omp` must be on `PATH` (on Windows `omp.exe` is preferred over `omp.cmd`), or
pass `--omp <path>` / set `OMP_DECK_OMP`.

Have `omp` publish its sessions without anyone typing `/collab`:

```sh
omp config set collab.autoStart control
```

## Usage

```sh
omp-deck serve [--bind ADDR:PORT] [--discord-webhook URL] [--config FILE]   # prints the URL on stdout
omp-deck list [--json]                                      # terminal table / parsed model
omp-deck self-update [--yes] [--check]                       # update the binary itself
```

- Without `--bind`, the server listens on this machine's Tailscale IPv4
  address (`tailscale ip -4`) on a port the OS picks. If Tailscale cannot be
  queried it falls back to `127.0.0.1` and says so on stderr. It never defaults
  to `0.0.0.0`.
- `GET /` is the dashboard, `GET /api/hosts` the host list as JSON (no links).
- With `--discord-webhook <URL>` (or `OMP_DECK_DISCORD_WEBHOOK`), `serve` polls
  `omp collab list` every 15 seconds and posts a Discord message with the
  **control** link the first time a session gets a title (`sessionName`), once
  per session. Sessions already titled when `serve` starts are treated as
  seen and not announced, so "once" holds across restarts. Untitled sessions
  and repeat polls are skipped. A failed poll or webhook call (including a
  non-2xx response) is logged to stderr, does not stop the server, and the
  session is retried on the next poll.
- Links are fetched per click via `GET /go/<instanceId>/<view|control>`, which
  runs `omp collab link` and answers `302` to the URL, so the secret never
  appears in the page, the JSON, or a cache.
- If `omp` is missing or fails, the page shows the error and the API returns
  `502`. An empty list means "no live omp sessions", not an error.
- Every request spawns `omp` fresh with a 5 second timeout. With `omp.cmd` on
  Windows a timeout may leave the child of the batch wrapper running.
- Every command checks GitHub for a newer release in the background (throttled
  to once every 24 hours) and prints a banner to stderr pointing at
  `omp-deck self-update` if one exists — never a silent install. Set
  `OMP_DECK_NO_AUTOUPDATE=1` to disable the check entirely.

## Starting a session from the dashboard

The **New session** panel lists local checkouts and starts
`omp --cwd <checkout> [--model <model>]` for the one you pick. There is no
initial prompt and no other option: send prompts from the collab room. The
session shows up as a normal card once omp publishes it, so omp must be
configured with `collab.autoStart control` (see above).

**The model is chosen at launch because it cannot be changed later.** omp's
collab guest permission model makes `/model`, `/compact`, `/resume`, `/branch`
and friends host-only, even for a full-control link, so from the phone the
launch is the only place to pick it.

Both lists come from an optional config file, `<config dir>/omp-deck/config.toml`
(`%APPDATA%\omp-deck\config.toml` on Windows, `~/.config/omp-deck/config.toml`
on Linux), or `--config <path>` / `OMP_DECK_CONFIG`. A missing default file is
fine; a `--config` path that does not exist is an error.

```toml
[repos]
# ghq layout: <root>/<host>/<owner>/<repo>. Empty by default: nothing is scanned.
roots = ['C:\Users\me\src', 'D:\ghq']

[models]
# Passed verbatim to `omp --model`, which fuzzy-matches. Empty: the model field
# is hidden and omp's default is used.
list = ["opus", "gpt-5.2"]
```

The file is a [teravars](https://github.com/yukimemi/teravars) Tera template
(`system.*`, `[vars]`), rendered before TOML is parsed, so write paths that
contain quotes as single-quoted TOML strings. Roots that are missing or
unreadable are skipped, and a checkout reachable through several roots is
listed once. The scan is cached for 30 seconds.

API: `GET /api/repos` (`{repos: [{name, path}], hint}`), `GET /api/models`,
`POST /api/sessions` with `{"path": "...", "model": "..."}` (`model` optional).
It answers `202` right away; an unknown path is `404`, a model that is not a
configured candidate is `400`, and a failed launch is `502` with the error.

### How omp is launched

`omp` is an interactive TUI. Tried by hand on Windows: with `CREATE_NO_WINDOW`,
`CREATE_NEW_CONSOLE` or `DETACHED_PROCESS` and null or inherited stdio, omp
starts and then exits within seconds (status 129, as if hung up); it stays
alive only with a real console of its own. Rust's `Command` always passes the
child explicit std handles, so instead omp-deck runs
`cmd /c start "" /min omp.exe --cwd ...`, which creates the console and
detaches omp from the server (restarting omp-deck does not end sessions). No
PTY/ConPTY dependency is needed. Trade-offs:

- Each session opens one (minimized) console window on the desktop of the
  user running omp-deck. Without an interactive desktop (a Windows service)
  this is unlikely to work.
- The launch check only sees `cmd` fail or exit non-zero within 2 seconds. A
  session that starts and then dies, or lives but never publishes (no
  `collab.autoStart`), still answers `202`.
- Arguments go through `cmd.exe`: each is quoted, and a path or model that
  contains `"`, `%`, `!`, a control character or a trailing backslash is
  refused. With `omp.cmd` instead of `omp.exe` there is one more wrapper.
- Non-Windows: omp is spawned directly in its own process group with no
  terminal. This is untested; if omp needs a tty there it exits at once and
  you get a `502`.

## Security

There is **no authentication**: the tailnet is the security boundary. Control
links carry write access to the session, and anyone on the tailnet who can
reach the dashboard can use them. Bind to a tailnet address you trust, or to
`127.0.0.1`; do not expose the port beyond it. The Discord webhook receives
**control** links too: anyone with access to that Discord channel gets write
access to the session.

The dashboard can also **start omp** on this machine, so anyone who can reach
it can do that too. It is bounded: the server only spawns omp in a checkout it
listed itself from the configured roots (anything else is `404`, the same
principle as `/go/<id>`, which only accepts an id omp just reported), and only
with a model named in the config (anything else is `400`; empty means omp's
default). Arguments are passed as an argument vector, never a shell string (on
Windows through `cmd /c start`, with the quoting described above). There is
still no authentication: the tailnet is the boundary. The request must be
`application/json`, so a web page on another origin cannot trigger it without
a CORS preflight, which the server never grants.

## License

MIT
