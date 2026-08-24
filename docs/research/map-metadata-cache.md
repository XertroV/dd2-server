# What map metadata is already cached

Research for [What map metadata is already cached](https://github.com/XertroV/dd2-server/issues/5), part of [Listed maps after player threshold](https://github.com/XertroV/dd2-server/issues/2).

Sources: `migrations/`, `src/nadeo.rs`, `src/queries/moderation.rs`, `src/moderation_http.rs`, `src/queries/context.rs`, `src/router.rs`. No production code was changed.

## Short answer

The server already caches display metadata in `map_meta` (`map_uid`, `author_id`, `map_name`, `thumbnail_url`, `fetched_at`, `updated_at`). A listed-maps route can `LEFT JOIN map_meta` on `map_uid` cheaply and safely (primary key). Coverage is **not** all custom maps: a row exists only after someone hits the moderation HTTP API for that UID and a Nadeo fetch succeeds. Name/author/thumbnail for never-moderated maps are missing and would need a Nadeo fetch (or a backfill). Became-listed time is not stored in `map_meta` or `maps`.

The plugin-facing `maps` table has a `name` per `(uid, hash)` from context reports. It is **not** 1:1 on UID and has no author or thumbnail.

## `map_meta` columns

Created by `migrations/20260726090000_map-moderation.up.sql`:

```sql
CREATE TABLE map_meta (
    map_uid VARCHAR(27) PRIMARY KEY NOT NULL,
    author_id UUID NOT NULL,
    map_name TEXT,
    thumbnail_url TEXT,
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX map_meta_author_id_idx ON map_meta (author_id);
```

Rust shape (`src/queries/moderation.rs`):

| Column | Type | Notes |
| --- | --- | --- |
| `map_uid` | `VARCHAR(27)` PK | Trackmania map UID. No FK to `maps`. |
| `author_id` | `UUID` NOT NULL | Nadeo map author WSID, not a display name. |
| `map_name` | `TEXT` nullable | Nadeo Live `name`. |
| `thumbnail_url` | `TEXT` nullable | Nadeo Live `thumbnailUrl`. |
| `fetched_at` | `TIMESTAMPTZ` | Set to `NOW()` on every upsert. Used as the freshness clock. |
| `updated_at` | `TIMESTAMPTZ` | Also set to `NOW()` on every upsert. Same write as `fetched_at`. |

Author display name is **not** a `map_meta` column. `GET /api/mod/map/<uid>` looks it up separately from `users.display_name` via `get_user_display_name` (`src/moderation_http.rs`, `src/queries/moderation.rs`). That join is null if the author has never appeared in `users` (plugin login / `ensure_user_reference`).

No later migration alters `map_meta`.

## When a `map_meta` row is created or updated

The only production writer is `upsert_map_meta` (`src/queries/moderation.rs`):

```sql
INSERT INTO map_meta (map_uid, author_id, map_name, thumbnail_url, fetched_at, updated_at)
VALUES ($1, $2, $3, $4, NOW(), NOW())
ON CONFLICT (map_uid) DO UPDATE SET
    author_id = EXCLUDED.author_id,
    map_name = EXCLUDED.map_name,
    thumbnail_url = EXCLUDED.thumbnail_url,
    fetched_at = NOW(),
    updated_at = NOW()
```

The only production caller is `ensure_map_meta` (`src/moderation_http.rs`):

1. If a row exists and `fetched_at` is newer than 7 days, return it.
2. If Nadeo is not configured, return the existing row (even if stale) or error `"map meta missing and Nadeo client not configured"`.
3. Else call `NadeoLiveClient::author_id_for_uid` and upsert.

`ensure_map_meta` is invoked from:

- `GET /api/mod/map/<uid>` (`handle_get_map`) — hard dependency; missing meta is `502`.
- `require_moderator` — used by ban and delegate routes on that UID. Failure is logged and author checks fail closed (`is_map_author` is false until a row exists).

There is no background job, no plugin-path writer, and no public `/map/<uid>/…` writer. Admin UI `web/admin.html` triggers the GET (map page load and `maybeEnrichUid` history enrichment).

## Freshness

- Cache TTL is **7 days**, checked only inside `ensure_map_meta`.
- A public list route that only `LEFT JOIN`s `map_meta` will see whatever was last upserted. It will **not** refresh stale or missing rows unless it also calls `ensure_map_meta` (one Nadeo HTTP GET per UID that is missing or older than 7 days).
- If Nadeo env vars are unset, stale rows are served forever and new UIDs cannot be populated (`src/nadeo.rs` warns and disables the client; `.env.example` documents `DD2_NADEO_DS_*`).
- `fetched_at` / `updated_at` are always rewritten together. There is no partial update of name-only or thumbnail-only.

## Coverage: not all custom maps

`map_meta` is a moderation cache, not a catalog of played maps.

Custom-map activity tables (`map_leaderboard`, `map_curr_heights`, `custom_map_stats`, minigames, collectibles, hats) store `map_uid` only. None of those queries write `map_meta`. `map_bans` / `map_delegates` also have **no FK** to `map_meta` (`found-issues.tmp.md` §23).

So a UID can be on a public Leaderboard (and therefore a future listed-maps set) with **no** `map_meta` row.

## Joining `map_meta` on a list route

Cheap and safe as a **left** join:

- `map_uid` is the primary key → one row or none.
- Secondary index exists only on `author_id` (for “my maps” / author lists).
- Existing list-style joins already do this: `list_delegated_maps` (`JOIN`), `list_active_bans_global` and `list_map_admin_delegates` (`LEFT JOIN`) in `src/queries/moderation.rs`.

Do **not** inner-join if the list source is `map_leaderboard` (or similar): that would drop never-moderated maps.

Do **not** call `ensure_map_meta` per list row. That is a Nadeo round-trip for every missing or 7-day-stale UID.

Name and author WSID are therefore “nice if present” via `LEFT JOIN map_meta`, not a schema change. Author **display name** is a further `LEFT JOIN users` on `author_id = web_services_user_id`, also optional.

## The `maps` table is a different thing

Created by `migrations/20240429044116_create_init_tables.up.sql`:

```sql
CREATE TABLE maps (
    map_id SERIAL PRIMARY KEY,
    uid VARCHAR(30) NOT NULL,
    name VARCHAR(255) NOT NULL,
    hash bytea NOT NULL,
    load_count INTEGER NOT NULL DEFAULT 1,
    created_ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW()
);
```

Uniqueness is `(uid, hash)` (`migrations/20240520205034_maps-unique-index.up.sql`), not `uid`. One UID can have many rows (different hashes / names).

Writer: `get_or_insert_map` (`src/queries/context.rs`), called from plugin context updates (`src/server2.rs` `XPlayer::update_context`). The plugin `Map` payload is `{ uid, name, hash }` (`src/router.rs`). On conflict, only `load_count` increments; `name` is not refreshed.

`maps` is used to attach a `map_id` to `contexts` and to export session history (`src/queries/stats.rs` joins `maps` on `map_id`). It is not used by moderation or public custom-map HTTP.

Implications for listed-maps:

- Plugin `name` exists for maps that reported a context, but joining on `uid` is not 1:1.
- No author, no thumbnail, no Nadeo fetch.
- `uid` is `VARCHAR(30)` here vs `VARCHAR(27)` on `map_meta` / `map_leaderboard`.

## Nadeo fetch: what is requested vs stored

`src/nadeo.rs` is a dedicated-server Live Services client. It GETs:

`https://live-services.trackmania.nadeo.live/api/token/map/{map_uid}`

Deserialized (`LiveMapInfo`):

| JSON field | Rust field | Stored? |
| --- | --- | --- |
| `uid` | `uid` | Already the lookup key. |
| `author` | `author` (string → UUID) | Yes, `map_meta.author_id`. |
| `name` | `name` | Yes, `map_meta.map_name`. |
| `thumbnailUrl` | `thumbnail_url` | Yes, `map_meta.thumbnail_url`. |

No other Nadeo fields are deserialized. There is no batch endpoint, no display-name endpoint, and no write of the raw JSON.

## What a listed-maps route would still have to fetch (or backfill)

Required by the parent map (UID + became-listed time):

- **UID** — already on `map_leaderboard` / other custom-map tables. Not a metadata problem.
- **Became-listed time** — not stored in `map_meta` or `maps`. Out of scope for this cache; see sibling tickets on the listing flip.

Nice-to-have display fields:

| Field | Already cached? | Gap |
| --- | --- | --- |
| Map name | Yes, in `map_meta.map_name`, only after admin+Nadeo | Missing for never-moderated UIDs. Plugin `maps.name` is a non-unique fallback, not Nadeo. |
| Author WSID | Yes, `map_meta.author_id` | Same coverage hole. |
| Author display name | Only if that WSID is in `users` | Not fetched from Nadeo. Null for authors who never logged into this server. |
| Thumbnail URL | Yes, `map_meta.thumbnail_url` | Same coverage hole. Not in `maps`. |

Filling gaps for a complete list means either:

1. Ship the list with null name/author/thumbnail for missing `map_meta` rows (no Nadeo, cheap join), or
2. Backfill / lazily fetch via the existing `ensure_map_meta` path (Nadeo GET per UID; not cheap on the request path).

Including name/author when the join hits is not a big change. Making them always present is a Nadeo coverage problem, not a schema problem.
