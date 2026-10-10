# Daemon and protocol

`omavrythingd` is the index daemon. In the current walking skeleton it scans a
root directory into a minimal in-memory index and serves queries over a Unix
domain socket.

## Running

```sh
scripts/run-daemon.sh                              # indexes $HOME
scripts/run-daemon.sh --root /home/sam/Documents   # indexes a subtree
```

The socket defaults to `$XDG_RUNTIME_DIR/omavrything.sock` (falling back to
`/tmp/omavrything.sock`). The daemon prints the indexed entry count and the
socket path on startup.

## Protocol

Newline-delimited JSON over the Unix socket: one request per line, one response
per line.

Request:

```json
{"q":"report 2024","limit":8}
```

- `q` — the query. Space-separated terms are ANDed (case-insensitive substring
  over the full path).
- `limit` — optional; defaults to 100, capped at 1000.

Response:

```json
{"results":[{"path":"/home/sam/report-2024.pdf","is_dir":false}],"total":1,"took_ms":0}
```

## CLI

```sh
omavry search "report 2024"
```

## Current limitations (walking skeleton)

- The index is built once at startup; there are no live updates yet.
- No persistence; the index is rebuilt on every start.
- No excludes, parallelism, or mount discovery.
- The daemon runs as the user; the root system service arrives later.
