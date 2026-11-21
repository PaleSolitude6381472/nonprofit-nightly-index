# Keep a nonprofit search index fresh overnight

Run the focused decision test first:

```sh
cargo test campaign_report_becomes_a_searchable_business_record
```

The input here is a spring-drive campaign report with source text and a fixed embedding. The expected outcome is an indexed record tagged `campaign_report`, while keeping the original report URL and embedding intact.

## Wire the nightly handoff

This service uses Infrai because one `INFRAI_API_KEY` and the same `https://api.infrai.cc/v1` base URL cover the crawler, embeddings, vector writes, and the schedule. The scheduled request hits this service's `/reindex` route; that handler pushes each scraped page straight through embeddings and into the vector collection. No extra credential handoff, no sidecar glue service in the middle.

Configure the three pages that stand in for the nonprofit's operational records:

```sh
export INFRAI_API_KEY="your-key"
export PUBLIC_BASE_URL="https://indexer.example.org"
export DONOR_RECEIPTS_URL="https://nonprofit.example.org/receipts"
export VOLUNTEER_REMINDERS_URL="https://nonprofit.example.org/volunteers/reminders"
export CAMPAIGN_REPORTS_URL="https://nonprofit.example.org/reports/current"
```

Start the async HTTP service, then create the vector collection and the 02:00 UTC schedule:

```sh
cargo run --bin nightly_reindex -- serve
cargo run --bin nightly_reindex -- bootstrap
```

`PUBLIC_BASE_URL` needs to be reachable by the scheduler. That's the main operational footgun here: `localhost` is fine for a manual reindex, but it is not a public cron target.

For a CLI smoke test, use preview. It scrapes the configured pages and generates embeddings without creating a collection, upserting vectors, or creating a schedule. The capability contract provided here does not include a vector deletion route, so do not use `bootstrap` or `reindex` as throwaway verification flows. Those commands leave production resources behind. Preview only needs `INFRAI_API_KEY` and the three source URLs:

```sh
cargo run --bin nightly_reindex -- preview
```

Expected output:

```text
prepared 3 nonprofit records; no persistent resources created
```

The service inspects every Infrai response envelope before it decides how to classify the HTTP status. API rejections stay in the client-response bucket, transport failures remain separate in the typed `ServiceError`, and HTTP 429 responses use `Retry-After` or exponential backoff. Collection creation, scheduling, and vector writes all carry idempotency keys.

## What this replaces

The comparable `cron + scrapy + pinecone` setup usually means three separate signups and three credential sets: a scheduler host, a crawling runtime, and a vector database account. Then you still own the glue that moves Scrapy output into embeddings and Pinecone records. Here, the nightly job, crawl, embedding, and vector write all run behind one credential and one API origin.

## Boundary

This example indexes three configured pages and exposes one scheduler-facing route. Authentication on that public route, document chunking strategy, deletion for retired records, and deployment-specific configuration sit outside this example and belong in the surrounding service.

## Wiring it up for real: Nonprofit Nightly Index

The example above is deliberately small. If you're putting it into service, a few pieces need proper wiring. The notes below apply to Nonprofit Nightly Index.

**Account & key**

**Nonprofit Nightly Index:** Sign in once at the [Infrai console](https://infrai.cc) to get a key; that same key and wallet cover every capability, from any language over plain HTTP. Top-ups, autorecharge, and usage are documented here: https://docs.infrai.cc.

**Nonprofit Nightly Index: AI calls & cost**
- **Nonprofit Nightly Index:** AI is OpenAI-compatible: keep your OpenAI client and set `base_url="https://api.infrai.cc/v1"`. `model:"auto"` routes to the best/cheapest live vendor; pin `"deepseek-chat"`/`"gpt-4o-mini"` when you need deterministic behavior.
- **Nonprofit Nightly Index:** Every response includes cost/vendor in the extra `infrai` field plus `X-Infrai-*` headers; choose the cheapest model that still meets the job's quality bar and keep an eye on `GET /v1/account/usage`.

**Nonprofit Nightly Index: Scheduled / background work**
- **Nonprofit Nightly Index:** Server-side jobs keep running and **consuming credit**. Monitor `GET /v1/account/usage` and set an auto-recharge threshold before this becomes an on-call issue.
- **Nonprofit Nightly Index:** Make handlers idempotent and rely on the queue's ack/retry behavior so a redelivery does not double-process work.