# rust-scraper

[![API docs](https://img.shields.io/badge/API_docs-Redoc-8A2BE2)](https://david-szczepanik.github.io/rust-scraper/)
[![OpenAPI 3.1](https://img.shields.io/badge/OpenAPI-3.1-85EA2D?logo=openapiinitiative)](openapi.json)

rust-scraper is an HTTP service that fetches Czech court decisions by case number or search term and stores them in PostgreSQL.

| Court key | Court | Source |
| :--- | :--- | :--- |
| `ustavni` | Constitutional Court (Ústavní soud) | `nalus.usoud.cz` |
| `nejvyssi` | Supreme Court (Nejvyšší soud) | `rozhodnuti.nsoud.cz` |
| `nejvyssi_spravni` | Supreme Administrative Court (Nejvyšší správní soud) | `vyhledavac.nssoud.cz` |

Before scraping, it looks the case up in the `judikatura` table. A case that is already there is not scraped again, and the response only returns its stored case number. Newly scraped cases are saved to the table.

## Quickstart (Docker)

```bash
docker run -d -p 8080:8080 -v scraper_logs:/_LOGS -e DATABASE_URL="postgres://user:pass@host:5432/db" ghcr.io/david-szczepanik/rust-scraper:latest
```

```bash
curl http://localhost:8080/health
```

`build-and-push.ps1` builds the published image for `linux/arm64` locally. The `build-push` workflow builds `linux/amd64` and `linux/arm64`, but only when started by hand.

### Running locally

1. Clone the repository.

   ```bash
   git clone https://github.com/David-Szczepanik/rust-scraper.git
   ```

2. Create `.env` from the example and set `DATABASE_URL`, e.g. `postgres://scraper:secret@localhost:5432/pravnianalyza`.

   ```bash
   cp .env.example .env
   ```

3. Optional, on Windows: start a local Postgres in Docker. `docker-start.bat` creates the `scraper-db` container with the user, password and port from `DATABASE_URL`, and runs `db/init.sql` on first start. Without `DATABASE_URL` the service still runs, but it scrapes every case and saves nothing.

4. Start the server with `cargo run`, or `0-run.bat` on Windows.


## Environment variables

| Variable | Description |
| :--- | :--- |
| `DATABASE_URL` | Postgres connection string. |
| `PORT` | HTTP port. Default `8080`. |
| `RUST_LOG` | Log filter for `tracing`, e.g. `debug`. |
| `DEBUG` | `1` scrapes only the first case per court. |

## API

Full reference with schemas and examples: **[API docs](https://david-szczepanik.github.io/rust-scraper/)**. The spec is [`openapi.json`](openapi.json).

| Method | Path | Description |
| :--- | :--- | :--- |
| `GET` | `/health` | Liveness check and version. |
| `POST` | `/cases` | Fetch decisions by case number and store them. |
| `GET` | `/cases/search` | Search by phrase or keyword in the listed courts. |

### `POST /cases`

```bash
curl -X POST http://localhost:8080/cases -H "Content-Type: application/json" -d '{
  "task_id": "001",
  "ustavni": ["Pl. ÚS 19/93"],
  "nejvyssi": ["3 Tdo 706/2024"],
  "nejvyssi_spravni": ["1 As 100/2020"]
}'
```

### `GET /cases/search`

Repeat a parameter to pass several values.

```bash
curl -G http://localhost:8080/cases/search --data-urlencode "court=ustavni" --data-urlencode "court=nejvyssi" --data-urlencode "phrase=právo na spravedlivý proces" --data-urlencode "keyword=restituce" --data-urlencode "limit=5"
```

Only the Constitutional Court gets a real full-text search, returning up to `limit` hits per term. For `nejvyssi` and `nejvyssi_spravni`, each term is treated as a case number and scraped directly. Cases found this way are stored too.

### Responses

Both endpoints return the case numbers found, grouped by court key, and the failures. A failure key is a case number for `POST /cases` and a court key for search.

```json
{
  "task_id": "123",
  "scraped_count": 2,
  "results": { "ustavni": ["Pl. ÚS 19/93"], "nejvyssi": ["3 Tdo 706/2024"] },
  "failed": { "1 As 100/2020": "Case not found" }
}
```

| Status | Meaning |
| :---: | :--- |
| `200` | Every case succeeded. |
| `207` | Some cases failed, some succeeded. |
| `422` | Nothing to do: no cases, no courts, no search terms, or an unknown court. Body is `{ "error": "..." }`. |
| `502` | Every case failed. The court websites are the upstream that failed. |
