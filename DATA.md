# Data

## Provenance

The snapshot in `data/` was collected from Hyperliquid's **public, read-only
Info API** (`POST https://api.hyperliquid.xyz/info`, the endpoint documented
for market data queries such as funding history). No authentication, no
trading, no account access — only public market data. The collector respected
the API's rate limiting (429 responses were handled with patient backoff
waits rather than retries that would burn rate-limit budget).

## Snapshot contents

| File | What it is |
|---|---|
| `funding_history.db` | SQLite database, table `funding_history(ts INTEGER, coin TEXT, funding_rate REAL, premium REAL, PRIMARY KEY(ts, coin))` |
| `meta.json` | Collection provenance (copied verbatim below) |
| `CHECKSUMS.sha256` | SHA-256 checksums of the two files above |

`meta.json` (verbatim):

```json
{
  "pulled_at_utc": "2026-10-27T22:35:30Z",
  "start_time_utc": "2026-09-12T22:29:18Z",
  "end_time_utc": "2026-10-27T22:29:18Z",
  "days": 45,
  "universe_count": 235,
  "coins_fetched": 179,
  "coins_skipped": [],
  "coins_errored": [],
  "coins_filter": null,
  "delisted_excluded": 56,
  "row_count": 192256,
  "api": "hyperliquid-info"
}
```

Summary:

- **Window:** 2026-09-12T22:29:18Z → 2026-10-27T22:29:18Z (45 days, hourly
  funding rates)
- **Universe:** 235 coins considered; 56 delisted coins excluded up front;
  179 coins fetched; 0 skipped; 0 errored
- **Rows:** 192,256
- **Pulled:** 2026-10-27

## Why frozen

The file is frozen — byte-identical, checksum-pinned — so that every
published number is **byte-for-byte reproducible**. The simulator anchors its
45-day window to the newest timestamp *in the file* (not the wall clock), so
the same file plus the same flags always produce the identical JSON output.
If new data were pulled later, that would be a different snapshot and
different numbers; it would never silently change the results documented in
the README.

Verify integrity with:

```sh
shasum -a 256 -c data/CHECKSUMS.sha256
```

## Privacy, terms, and licensing of the data

- **No personal data.** The snapshot contains only coin symbols, timestamps,
  and funding rates — public market data with no account, address, or
  personal information of any kind.
- **Terms of service.** Collection used only the read-only public market-data
  endpoint, without authentication, at rates the API accepted, per
  Hyperliquid's published terms (https://hyperliquid.xyz). If you reuse this
  data, review those terms yourself; redistribution of the snapshot in this
  repository is for reproducibility of the research artifact.
- The data is provided **as-is**, with no warranty; see `LICENSE`.
