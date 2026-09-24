<img src="assets/logo.svg" alt="omp-deck logo" width="64" height="64" align="right">

# omp-deck

A small CLI that serves a web dashboard of the live
[`omp`](https://github.com/can1357/oh-my-pi) collab sessions on this machine,
so you can open any of them from your phone.

One card per session: name, working directory, model, a busy / input-required /
idle badge, participants and how long ago it started, with **view** and
**control** links that open the `my.omp.sh` room.

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
omp-deck serve [--bind ADDR:PORT]   # prints the URL on stdout
omp-deck list [--json]              # terminal table / parsed model
```

- Without `--bind`, the server listens on this machine's Tailscale IPv4
  address (`tailscale ip -4`) on a port the OS picks. If Tailscale cannot be
  queried it falls back to `127.0.0.1` and says so on stderr. It never defaults
  to `0.0.0.0`.
- `GET /` is the dashboard, `GET /api/hosts` the host list as JSON (no links).
- Links are fetched per click via `GET /go/<instanceId>/<view|control>`, which
  runs `omp collab link` and answers `302` to the URL, so the secret never
  appears in the page, the JSON, or a cache.
- If `omp` is missing or fails, the page shows the error and the API returns
  `502`. An empty list means "no live omp sessions", not an error.
- Every request spawns `omp` fresh with a 5 second timeout. With `omp.cmd` on
  Windows a timeout may leave the child of the batch wrapper running.

## Security

There is **no authentication**: the tailnet is the security boundary. Control
links carry write access to the session, and anyone on the tailnet who can
reach the dashboard can use them. Bind to a tailnet address you trust, or to
`127.0.0.1`; do not expose the port beyond it.

## License

MIT
