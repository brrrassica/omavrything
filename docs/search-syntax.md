# Search syntax

`omavrything` uses a deliberately small query language: a query is a list of
whitespace-separated **terms**, and every term must match. There are no
wildcards, regular expressions, boolean operators, or field prefixes yet.

## Terms are ANDed

A query is split on whitespace into terms. A file matches only when **every**
term is found somewhere in its full path:

| Query | Matches |
| --- | --- |
| `report` | Any path containing `report`. |
| `report 2024` | Paths containing **both** `report` and `2024`, in any order. |
| `report 2024 pdf` | Paths containing all three terms. |

Term order does not matter: `2024 report` and `report 2024` return the same
files. An empty or whitespace-only query matches nothing.

## Matching is a case-insensitive substring

Each term is matched as a **case-insensitive substring** of the full path
(parent directories plus the file name). It is not anchored, so a term can match
anywhere:

| Query | Matches |
| --- | --- |
| `report` | `report.pdf`, `2024-report.txt`, `my-reports/` |
| `REPORT` | The same files — matching is case-insensitive. |
| `doc` | `Documents/`, `doc.txt`, `mydoc/` |

Matching is over the **whole path**, not just the file name, so a term can match
a directory component:

```text
Query:  documents report
Path:   /home/sam/Documents/2024/report.pdf   ✓ (both terms present)
Path:   /home/sam/report.pdf                  ✗ (no "documents")
```

## Terms can span the path separator

A term may contain `/` and match across the boundary between a directory and its
child, so `documents/report` matches `/home/sam/Documents/report.pdf`:

| Query | Matches |
| --- | --- |
| `documents/report` | `/home/sam/Documents/report.pdf` |
| `2024/report` | `/home/sam/2024/report.pdf` |

This is a plain substring match, not a path-segment match: `documents/report`
also matches `/home/sam/mydocuments/report.pdf`.

## Unicode

Non-ASCII terms are matched with a Unicode-aware lowercase comparison, so
`ÜNÏCODE` matches `ünïcode.txt`. ASCII terms use a faster byte-wise search.

## Ranking

Results are ranked, best first, by:

1. **Name match** — files whose terms all match within the basename rank above
   files that only match in a parent directory.
2. **Match position** — an earlier match in the path ranks higher.
3. **Path length** — a shorter full path ranks higher.
4. **Depth** — a shallower path ranks higher.
5. **Recency** — a more recently modified file ranks higher.

The ranking is applied by the daemon. In the scratchpad you can re-sort the
returned page by clicking a column header (name, size, modified, path); a fresh
query restores the daemon's ranking.

## Paging

The protocol returns a bounded page. The request carries `limit` (default 100,
capped at 1000) and `offset` (the number of leading matches to skip), and the
response reports `total`, the number of **all** matching entries, so a client can
show "showing X of Y" and page through the rest.

## From the CLI

```sh
omavry search "report 2024"        # prints matching paths, one per line
omavry search documents/report     # a term spanning the separator
```

The CLI prints the matching paths to stdout and a `N of M result(s) in T ms`
summary to stderr, so it composes with other tools:

```sh
omavry search "invoice 2024" | head -n 5
```

> `omavry search --json` prints the same response as JSON, and the `rescan` and
> `mount` subcommands are available over D-Bus (see
> [`daemon.md`](daemon.md#protocol)).

## What is not supported

- **Wildcards** (`*.pdf`) — a `*` is matched literally.
- **Regular expressions** — patterns are plain substrings.
- **Boolean operators** (`OR`, `NOT`, `-term`) — all terms are ANDed; there is
  no negation or alternation.
- **Field prefixes** (`ext:pdf`, `size:>1M`) — not parsed; `ext:pdf` is matched
  as the literal substring `ext:pdf`.
- **Quoted phrases** — quotes are matched literally, not as grouping.

These may arrive in a later release; the query parser is intentionally minimal
for v0.1.
