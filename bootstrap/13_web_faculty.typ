= Web: Search and Fetch Through Provider APIs

`web.rs` is the agent's web access faculty. Backed by Tavily or
Exa (provider configured via API key). Two operations: `search`
to query the web, `fetch` to pull and clean a single URL.

== Why this exists

  - Direct `curl` from the agent loses provenance — the bytes
    arrive but no record of where they came from. `web.rs`
    persists each request as a pile event with the URL, query,
    timestamp, and response.
  - Provider APIs (Tavily / Exa) extract clean
    text/markdown from cluttered HTML, which beats raw scraping
    for downstream processing.
  - The pile branch becomes a queryable history: "have I
    already pulled this URL? what did it say?" answered without
    re-fetching.

== Usage

```sh
# Provider key in env (Tavily example)
export TAVILY_API_KEY=tvly-...

# Search
web.rs search "succinct hash array mapped trie"

# Fetch a URL (clean markdown when the provider supports it)
web.rs fetch https://arxiv.org/abs/2305.12345
```

`fetch` returns clean text. If you want the original bytes
(PDFs, datasets), use `files.rs fetch <url>` instead — that
archives the raw response under a content hash.

== Coordination with files

A common pattern:

  + `web.rs search "<query>"` — find candidate URLs.
  + Pick a result.
  + `files.rs fetch <url>` — archive the raw bytes
    (`files:<hash>` returned).
  + `wiki.rs create "..." --tag paper` — write a fragment
    citing the `files:<hash>`.

So web.rs is the discovery / clean-extract step; files.rs is
the durable-archive step. Use the right one for each job.

== When NOT to use it

  - Pages that need authentication or interactive
    JavaScript — provider APIs handle static content well, but
    SPA-heavy pages may return shells.
  - Bulk crawling — the provider cost adds up; `wget`/`curl` +
    files.rs is cheaper at volume.
  - Anything you've already pulled this session — check the
    pile's web events branch first.

== Branch and storage

Each `search` and `fetch` records an event on the pile's web
branch (configurable via `--branch-id`). Events accrete; nothing
is overwritten. Querying "what did I fetch about X" is a wiki-
or pattern-style query against the branch.
