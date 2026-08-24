# How custom maps are distinguished from Deep Dip 2

Research for [How custom maps are distinguished from Deep Dip 2](https://github.com/XertroV/dd2-server/issues/3), part of [Listed maps after player threshold](https://github.com/XertroV/dd2-server/issues/2).

Sources: `src/`, `migrations/`, `CONTEXT.md` on this branch (`origin/master` at write time). No listed-maps route exists yet.

## Answer in one paragraph

Deep Dip 2 is a single hardcoded UID (`DD2_MAP_UID` = `DeepDip2__The_Storm_Is_Here`). Its official height Leaderboard, stats, and finishes live on **global, un-namespaced** tables and routes (`leaderboard`, `/leaderboard/global`, socket `GetGlobalLB` / `ReportPBHeight`). Every other climb uses the **per-UID** tables and the `/map/<uid>/…` route family, keyed by a 27-character Trackmania UID. There is no UID allow-list: the first `ReportMapCurrPos` for a 27-char UID registers it in `map_leaderboard` / `map_curr_heights`. The listed-maps route should scan `map_leaderboard` and **exclude** `DD2_MAP_UID`. Shallow Dip (`DeepDip2__The_Gentle_Breeze`) is already in that per-UID family; it is not a second global map.

---

## How Deep Dip 2 is identified

### UID constant

The only first-class map constant is:

```1:1:src/consts.rs
pub const DD2_MAP_UID: &str = "DeepDip2__The_Storm_Is_Here";
```

That string is 27 characters (Trackmania UID length).

Identity checks:

| Site | Test | Effect |
| --- | --- | --- |
| `Map::is_dd2` (`src/router.rs`) | `uid.starts_with(DD2_MAP_UID)` | Marks the current map as DD2 for official-context |
| `PlayerCtx::is_official` (`src/router.rs`) | `is_dd2()` **and** not-editor flags (`check_flags_sf_mi`) **and** `e` is not true | Official DD2 climb (vehicle/PB/stats accepted) |
| `UserContext::is_dd2_uid` (`src/queries/stats.rs`) | `map_uid == DD2_MAP_UID` | Exact match on stored context |
| `XPlayer::on_report_pb_height` (`src/server2.rs`) | `map.uid != DD2_MAP_UID` | Drops `ReportPBHeight` for any other map |
| `report_live_vehicle_state` (`src/queries/stats.rs`) | writes `map_curr_heights` with `DD2_MAP_UID` when official | Mirrors DD2 live presence onto the per-UID live table |
| `get_live_leaderboard` (`src/queries/stats.rs`) | reads `get_map_live_heights(pool, DD2_MAP_UID)` | Global `/live_heights/global` is DD2-only |
| `map_events` seed / `get_secret_assets` | literal `'DeepDip2__The_Storm_Is_Here'` | DD2 finish-event / secret assets |

`starts_with` vs `==` is equivalent for a valid 27-char UID. `is_official` is **not** a generic “this map is official” flag; it means “this session is an official Deep Dip 2 climb.”

### Special tables (no `map_uid`)

These are DD2-only by schema: one row per player, no map column.

- `leaderboard` / `leaderboard_archive` / view `ranked_lb_view` — height PB. Created in `migrations/20240429044116_create_init_tables.up.sql`, ranked in `migrations/20240503043231_ranked_lb_view.up.sql`. Written by `update_user_pb_height`.
- `stats` / `stats_archive` — DD2 climb stats (`ReportStats` / `GetMyStats`). Official-context only (`XPlayer::on_report_stats` drops unofficial).
- `vehicle_states` (`is_official` column) — DD2 live telemetry.
- `falls`, `finishes`, `respawns` — DD2 session events.
- `map_events` — seeded only for the DD2 UID (`1finish` / `2finish` / `3finish`) in `migrations/20240530045338_profile-settings.up.sql`.

### Global vs per-map routes

Un-namespaced HTTP routes in `src/http.rs` (`core_routes`) are the DD2 public API:

- `/leaderboard/global`, `/leaderboard/global/<page>`, `/leaderboard/global/len`, `/leaderboard/<wsid>`
- `/live_heights/global`, `/live_heights/<wsid>`
- `/overview`, `/server_info`

Matching socket messages (`src/router.rs`): `GetGlobalLB`, `GetGlobalOverview`, `GetMyRank`, `GetPlayersPb`, `ReportPBHeight`, `ReportVehicleState`, `ReportStats`.

The `/map/<uid>/…` family is **not** closed to DD2. Handlers take any UID of plausible length. Tests in `src/queries/custom_maps.rs` assert that `/map/{DD2_UID}/live_heights` returns official DD2 live rows. So DD2 is dual-pathed for **live heights** (`map_curr_heights` + global `/live_heights/global`), but its **PB Leaderboard** stays on `leaderboard`, not `map_leaderboard`.

Product language (`CONTEXT.md`): Deep Dip 2 is the founding event; a **Map** is any Dips++-enabled climb identified by UID; the global height list is the special case.

---

## Other official / legacy maps

### Shallow Dip — `DeepDip2__The_Gentle_Breeze`

The only other hardcoded map UID in this tree.

- Live count is injected into the DD2 global overview as `nb_climbing_shallow_dip` via `get_map_nb_playing_live(pool, "DeepDip2__The_Gentle_Breeze")` in `src/queries/stats.rs`.
- There is **no** `SHALLOW_DIP_MAP_UID` constant, no dedicated table, no global `/leaderboard` twin.
- `migrations/20240527032643_shallow-dip-heights.up.sql` originally sketched `sd_curr_heights` / `sd_pb_leaderboard`, then created the generic `map_curr_heights` / `map_leaderboard` instead (`map_uid VARCHAR(27)`). Shallow Dip is the first consumer of the per-UID path, not a second DD2.

For listed-maps: Shallow Dip **is** in the `/map/<uid>/…` family. Issue 2 only says Deep Dip 2 need not appear.

### Deep Dip 1

No UID, table, or route for Deep Dip 1 on this branch. (Unpublished local `master` hat-unlock tests mention `DeepDip1___________________` as a `recordUids` string only — not a map in this server.)

### `maps` (context-load catalog)

`maps` (`migrations/20240429044116_create_init_tables.up.sql`, written by `get_or_insert_map` on `ReportContext`) stores every map a client has loaded, including DD2. It is not the custom-map standing catalog. Admin view `adm__contexts_in_editor` hardcodes the DD2 UID as one editor-detection hint.

---

## What makes a map belong to `/map/<uid>/…`

Membership is **not** an allow-list. A map is in the family if a client speaks the per-UID protocol with a 27-character UID.

### Write path (registers the map)

`Request::ReportMapCurrPos { uid, pos, race_time }` (`src/router.rs`, type 64, comment `// arbitrary maps`) → `XPlayer::report_map_curr_pos`:

1. Reject `uid.len() != 27`.
2. Upsert `map_curr_heights`.
3. Raise-or-insert `map_leaderboard`.

Same 27-char gate on minigame / collectible reports. Aux specs (`custom_map_aux_specs`) are keyed by `(user_id, name_id)`, **not** `map_uid` — they do not register a map.

### HTTP / socket read path

HTTP (`src/http.rs` `map_routes`, listed on `/api/routes`):

- `/map/<uid>/nb_active_climbers`
- `/map/<uid>/leaderboard`, `/leaderboard/len`, `/leaderboard/<page>`
- `/map/<uid>/live_heights`
- `/map/<uid>/minigames`, `/map/<uid>/minigame/<slug>/leaderboard…`
- `/map/<uid>/collectibles`

Handlers in `src/queries/api.rs` (`handle_get_map_uid_*`) pass `uid` through with **no DD2 exclusion**.

Socket twins: `GetMapOverview`, `GetMapLB`, `GetMapLive`, `GetMapMyRank`, `GetMapRank` (`src/router.rs` types 192–196). `GetMapOverview` uses `get_map_overview_player_counts` — public `map_leaderboard` size + live count — which issue 2 names as the listing-threshold count.

Length checks on read are slightly looser than the write gate (`len > 30` reject on LB; `20..30` on live heights). Schema is `VARCHAR(27)`.

### Per-UID tables

| Table | Role |
| --- | --- |
| `map_curr_heights` | Live position per `(map_uid, user_id)` |
| `map_leaderboard` | Height Standing per `(map_uid, user_id)` — Map Overview LB count |
| `custom_map_stats` | Per-map JSON stats (`ReportMapStats`) |
| `minigame_on_map` / `minigame_pb` / `minigame_attempt` | Minigames bound to a UID |
| `collectible_on_map` / `collectible_progress` | Collectibles bound to a UID |
| `map_meta` / `map_bans` / `map_delegates` / `map_unofficial` | Moderation; populated when someone opens `/api/mod/map/<uid>` |

None of these exclude `DD2_MAP_UID`. Official DD2 vehicle reports **do** write `map_curr_heights` for that UID. Official DD2 PB does **not** write `map_leaderboard` (it writes `leaderboard`). A client could still `ReportMapCurrPos` for the DD2 UID and create `map_leaderboard` rows.

---

## What to scan for “already-registered custom maps”

Issue 2: listing applies to every already-registered custom map, not only maps with an aux spec; a player toward the threshold is a unique person on that map’s public height Leaderboard (Map Overview count).

**Scan this:**

```sql
SELECT DISTINCT map_uid
FROM map_leaderboard
WHERE map_uid <> 'DeepDip2__The_Storm_Is_Here';
```

That is the set `get_map_leaderboard_len` / `get_map_overview_player_counts` already count, minus DD2.

Public count for one UID (threshold input) is `get_map_leaderboard_len` in `src/queries/custom_maps.rs`: `COUNT(*)` on `map_leaderboard` for `$1`, excluding `shadow_bans` and active `map_bans`.

Do **not** use as the registration set:

- `custom_map_aux_specs` — not keyed by UID.
- `maps` — any loaded map, including DD2.
- `map_meta` — only maps someone moderated.
- `map_curr_heights` alone — includes official DD2 live rows even with no custom-map LB.
- `minigame_on_map` / `collectible_on_map` — subset of maps that bound an aux spec.

`map_curr_heights` is a useful complement only if “registered” should include maps that have live presence but no Standing yet. The current write path inserts both tables together, so `map_leaderboard` is enough for the threshold definition.

---

## Hardcoded UID allow / deny lists

There is **no** allow-list and **no** deny-list for `/map/<uid>/…`. Any 27-character string is accepted on write.

Hardcoded UIDs in this tree:

| UID | Where | Purpose |
| --- | --- | --- |
| `DeepDip2__The_Storm_Is_Here` | `DD2_MAP_UID` (`src/consts.rs`) | Official DD2 identity |
| `DeepDip2__The_Storm_Is_Here` | `map_events` seed; `get_secret_assets`; `adm__contexts_in_editor` | DD2 events / editor hint |
| `DeepDip2__The_Gentle_Breeze` | `get_global_overview` literal | Shallow Dip live count on DD2 overview |

Listed-maps should treat `DD2_MAP_UID` as the deny entry. Shallow Dip stays unless a later ticket says otherwise.

---

## Implication for the listed-maps route

1. Cover the `/map/<uid>/…` family only: rows in `map_leaderboard` with `map_uid <> DD2_MAP_UID`.
2. Do not require an aux spec.
3. Threshold count = `get_map_leaderboard_len` (same as Map Overview `nb_players_on_lb`).
4. Do not add DD2 from `leaderboard` / `/leaderboard/global`.
5. No other UID constant needs excluding on this branch.
