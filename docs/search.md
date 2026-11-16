# Search

A search runs over one table of one branch.

```
find docs where team = core match billing error near embedding [0.1, 0.3, …] limit 10 offset 0
```

| Part | Meaning |
|---|---|
| `where f = v and …` | exact match on top-level fields: strings, numbers, booleans, null. Values are compared as JSON, so `1` and `1.0` differ. `f->>k` matches key `k` of an object field `f` as SQL's `->>` reads it (as text). |
| `match words` | text search (BM25) over all string fields, with typo tolerance |
| `near field [numbers]` | vector search: rank by dot product with a numeric-array field |
| `limit`, `offset` | paging (default limit 20) |

The same query works from Rust (`Query`), HTTP (`find`, see [http-api.md](http-api.md)) and MCP (`find`).

## Results

- **Count first:** the reply gives `total`, the number of rows that pass the filters, then the hits.
- **Ranking:**
  - **Filters only:** hits are in key order.
  - **Text or vector:** best first, with a score.
  - **Both:** the two rankings are fused with reciprocal rank fusion, so a row good at both comes first.
- **Paging:** use `next_offset` (HTTP) or `offset` for the next page.

## Text

- **Tokens:** lowercase words, split on anything that isn't a letter or digit.
- **Scoring:** BM25 over all string fields of a row.
- **Typos:** a query word the table has never seen is matched to words one edit away (a wrong, missing or extra letter, or two swapped letters), at half weight. Words of 4 to 32 letters are corrected; longer ones (hashes, ids) only match exactly, since the index would hold every deletion of each.
  - Real words are never "corrected", so exact spellings rank first.
  - Words under 4 letters aren't corrected.

- **From SQL:** `search('docs', 'agent memory' [, k])` in FROM gives this index's best `k` rows (default 10), best first: `id`, `score` and `row` (jsonb), on the session's world. Join it or filter it like any table: `select d.title, s.score from search('docs', 'memory') s join docs d on d.id::text = s.id`. Postgres's full-text functions (`to_tsvector`, `@@`, `ts_rank`, with stemming and stop words) work too: see [SQL](sql.md).

## Vectors

- **Storage:** store embeddings as a JSON array of numbers in any field (`"embedding": [0.1, …]`), and normalize them if you want cosine similarity.
- **Dimensions:** every row's vector in a field must have the same length. Rows with a different length are left out of vector search for that field.
- **Two ways to search, picked per query, for recall first:**
  - **Exact scan:** the candidates' 1-bit codes are scanned on the cores no other query is using, and the best 64 per result rescored at full precision (more below 512 dimensions, where codes have fewer bits: 8× at 64, which took 1M synthetic vectors from 91% to 99.4% recall@10). On 76,000 real OpenAI embeddings (1,536 dimensions) that finds 99.98% of the true top 10 in about 0.6 ms. Small candidate sets (up to about 4 million numbers, such as a filter keeping a few thousand rows) and vectors of 32 dimensions or fewer are compared whole instead: exact.
  - **HNSW graph:** fields worth more than a quick scan (1M code words: about 44,000 vectors of 1,536 dimensions) get a graph, built in the background the first time they're searched. When it's built the graph tests itself: its top 10 against the scan's for 64 of its own vectors. A graph that scores 99% or better is used for searches over most of the table (no filter, or one keeping at least half the rows); a weaker one only once the scan would pass 8.4M code words (about 350,000 vectors of 1,536 dimensions), so recall isn't traded away while a scan is still cheap. How well a graph does depends on the data: 99.5% on clustered synthetic vectors (500,000 × 384: 0.41 ms, 99.8% recall), 97% on real embeddings of near-duplicate entities, which then scan. From 512 dimensions the graph is walked on the 1-bit codes, a few popcounts a step, and the finds rescored at full precision: about 6× faster than walking the floats, at the same recall. Its beam is tuned when it's built: the narrowest from 64 to 400 that finds 99.5% of the true top 10 for probes near its own vectors, else 400 (on 1M real 1,536-dimension embeddings: 1.0 ms for 98.5% recall, where a beam of 1,024 took 2.3 ms for 99.4%). `ef` in `find` and `SET hnsw.ef_search` in SQL widen it for more recall. A pass after building links every node nothing linked to, so every vector can be found.
  - **Memory:** a vector column keeps its numbers as floats while it holds at most 64M of them (256 MB: about 43,000 vectors of 1,536 dimensions, 4M of 16), and at 8 bits a number above that (each vector with its own scale, its largest number at ±127): 4 times smaller, 1.5 GB instead of 6.1 GB for 1M × 1,536. 8-bit numbers can't tell near-duplicates apart (ranked by them alone, 1M real embeddings kept 94% of the true top 10), so such an index proposes twice the rows asked for and ranks them from the rows' own vectors, reading them on the free cores: the same answers as floats, in the same time (1.34 ms against 1.32 ms at 1M, 98.6% against 98.7% of the true top 10). `CHRONOS_VECTOR_FLOATS` sets the limit (0: 8-bit for every column) and `CHRONOS_RERANK` the rows proposed per row asked for (default 2). Scores `find` returns are exact either way.
  - **Backup:** a graph walk that comes up short, or a graph still building, falls back to the scan.
  - **`CHRONOS_SCAN_WORDS`** (default 8388608) moves the line for weaker graphs: higher scans more (recall), lower hands over to the graph sooner (speed).
- **No waiting:** a graph builds on its own threads, so no query waits for it. One builds at a time.
- **Turning graphs off:** `CHRONOS_VECTOR=flat` makes every search scan, for comparisons.

## Branches

- **Changes show immediately:** a branch sees its own changes the moment they're written.
- **Shared indexes:** indexes are built for one version of a table and shared. A branch that has changed little since then searches that index plus its own changed rows, and those changes are prepared once per branch version.
- **Speed:** forks search at about the same speed as `main`.
- **Repeats:** an identical query on an unchanged branch is answered from a cache in microseconds.

## Speed

At 1M rows with 64-dimension vectors (8-core laptop), p99 was about 2 ms for filter + text + vector together. See [../BENCHMARKS.md](https://github.com/Abhishekxdg/Chronos-DB/blob/main/BENCHMARKS.md).

## Not yet

- **Range filters** (`price < 50`).
- **Per-field text search.**
- **Phrase queries.**
- **Graph build speed:** a build takes tens of seconds per 100k vectors of 384 dimensions on a laptop. Faster builds, and keeping a graph when a table changes a lot (today it's rebuilt with the table's index), come next.
