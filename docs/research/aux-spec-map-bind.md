# How aux specs bind to a map UID today

Research for [How aux specs bind to a map UID today](https://github.com/XertroV/dd2-server/issues/4) (parent: [Listed maps after player threshold](https://github.com/XertroV/dd2-server/issues/2)).

Sources: `dd2-server` at this branch's `origin/master` tip, plus the live plugin (`tm-dips-plus-plus`) and editor (`tm-dipspp-editor`) as the clients that point a map at a spec. Hats `bind_map_from_catalog` is **not** on this master.

## Answers

| Question | Answer |
| --- | --- |
| Durable server-side spec↔`map_uid` binding? | **No row on `custom_map_aux_specs`.** Feature catalogs can copy slugs onto `minigame_on_map` / `collectible_on_map` after a client report. Those rows store `(spec_user, name_id)`, not `spec_id`, and only exist if the spec has catalog entries **and** a player has reported. |
| When created (which request)? | Primary: socket `ReportMinigameMap` `{uid, spec_id}`. Secondary per-slug: `ReportMinigameAttempt`, `ReportCollectibleUnlock`, `ReportCollectibleCollect`. Upload does **not** bind. |
| One spec → many maps? | **Yes.** Bind is keyed by the reported `map_uid`; the same `spec_id` can be reported for many 27-char UIDs. |
| Many specs → one map? | **Per slug, last write wins** (`UNIQUE (map_uid, slug)`). Distinct slugs from different specs can coexist. Play-time load is typically one spec via a single map-comment `url=`. |
| Does upload include a map UID? | **No.** `ReportCustomMapAuxSpec` is `{id, name_id, spec}`. Editor JSON has no map UID field. |
| 24-hour lock | **Server-enforced** on `created_at + 24h` for both modify and delete. Editor only documents it. |

A listing threshold stored on an advanced spec does **not** reach a map through a spec table. The durable client pointer is the map comment URL. Server-side height registration (`ReportMapCurrPos` → `map_leaderboard` / `map_curr_heights`) is independent of aux specs.

## 1. Spec identity is `(user_id, name_id)`, not map UID

`custom_map_aux_specs` has no `map_uid` column. Unique key is `(user_id, name_id)`; `name_id` is `VARCHAR(32)`.

```2:11:migrations/20250628200041_custom-map-aux-specs.up.sql
CREATE TABLE custom_map_aux_specs (
    id BIGSERIAL PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    name_id TEXT NOT NULL,
    spec JSONB NOT NULL,
    hit_counter BIGINT NOT NULL DEFAULT 0, -- how many times this aux spec has been accessed.
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(user_id, name_id)
);
```

```9:14:migrations/20250629080156_custom-map-aux-nameid-limit.up.sql
ALTER TABLE custom_map_aux_specs
    ALTER COLUMN name_id TYPE VARCHAR(32);

ALTER TABLE custom_map_aux_specs
    ADD CONSTRAINT custom_map_aux_specs_user_id_name_id_key UNIQUE (user_id, name_id);
```

Queries key only on those columns (`src/queries/custom_map_aux_specs.rs`: `get_spec`, `upsert_spec`, `delete_spec`, `list_specs`). Stored JSON is mapper-authored; `specId` / `spec_id` are stripped on write and `specId` is injected only on GET.

## 2. Upload does not take a map UID

Socket request 65:

```128:132:src/router.rs
    ReportCustomMapAuxSpec {
        id: u32,
        name_id: String,
        spec: serde_json::Value,
    } = 65,
```

Handler upserts by the authenticated `user_id` + `name_id`, then rebuilds minigame/collectible **catalogs** (not `*_on_map`):

```1583:1608:src/server2.rs
    pub async fn report_custom_map_aux_spec(
        ...
        let spec_id = queries::custom_map_aux_specs::upsert_spec(pool, user_id, name_id, spec).await?;
        queries::minigames::replace_catalog(pool, spec_id, user_id, name_id, spec).await?;
        queries::collectibles::replace_catalog(pool, spec_id, user_id, name_id, spec).await?;
```

Plugin payload matches: `{id, name_id, spec}` (`tm-dips-plus-plus/src/Server/Messages.as` `ReportCustomMapAuxSpecMsg`). Editor upload is `MyAuxSpecs::Report_Async(name_id, j)` then sets

`{HttpBaseUrl}/aux_spec/{LocalUserWSID}/{name_id}.json`

(`tm-dipspp-editor/src/Ed/Components/ProjInfo.as` `UploadJsonToDppServer`). `ProjInfo.as` has no `mapUid` / `map_uid` field; `ToCombinedJson` is not keyed by map.

## 3. Client pointer: map comment `url=` (not a server row)

Editor `DipsSpec.GenerateComment` writes `url=<px_url>` inside `--BEGIN-DPP--` … `--END-DPP--` (`tm-dipspp-editor/src/Ed/CM_Editor_Main.as`). The wizard then overwrites `map.Comments` (`FinWiz_90_SetMapComment`).

Plugin parse (`tm-dips-plus-plus/src/CustomMaps/MapCustomInfo.as`):

1. `DipsSpec(mapComment)` reads the `url=` key.
2. `LoadFromUrl` GETs that URL and stores the JSON as `auxInfo`.
3. `CustomMap::TryLoadingCustomData` calls `Minigames::LoadFromAux(auxSpec, mapUid)` then `Minigames::MaybeReportMap()` (skipped for playground test specs).

HTTP GET is by author WSID + name_id, not map UID:

```234:242:src/http.rs
    let get_custom_map_aux_spec = warp::path!("aux_spec" / String / String)
        ...
            let name_id = name_id.trim_end_matches(".json").to_string();
            api::handle_get_custom_map_aux_spec(pool.as_ref(), wsid, name_id).await
```

```194:203:src/queries/api.rs
pub async fn handle_get_custom_map_aux_spec(...)
    ...
        Ok(Some(r)) => Ok(warp::reply::json(&super::custom_map_aux_specs::spec_json_with_id(r.id, &r.spec))),
```

That injected `specId` is `custom_map_aux_specs.id`. The plugin will not submit scores or `ReportMinigameMap` without it (`Runtime.as` `ParseSpecId` / `hasSpecId`; playground JSON has none).

Fallback if the GBX has no Dips comment: `CheckForUploadedMapData` GETs `https://assets.xk.io/d++maps/{mapUid}.txt` and treats the body as a map comment. That is **not** `dd2-server` and still only yields a comment (typically another `url=`).

## 4. Server bind path: `ReportMinigameMap` → `bind_map_from_catalog`

Socket request 81:

```152:155:src/router.rs
    ReportMinigameMap {
        uid: String,
        spec_id: i64,
    } = 81,
```

```1657:1670:src/server2.rs
    pub async fn report_minigame_map(...)
        if map_uid.len() != 27 { return Ok(()); }
        if spec_id <= 0 { return Ok(()); }
        let _ = queries::minigames::bind_map_from_catalog(pool, &map_uid, spec_id).await?;
        let _ = queries::collectibles::bind_map_from_catalog(pool, &map_uid, spec_id).await?;
```

Plugin send site (`tm-dips-plus-plus/src/Minigames/Runtime.as`):

```1102:1105:tm-dips-plus-plus/src/Minigames/Runtime.as
    void MaybeReportMap() {
        if (!g_rt.hasSpecId) return;
        if (IsSilentForSocket()) return;
        PushMessage(ReportMinigameMapMsg(g_rt.mapUid, g_rt.specId));
```

`bind_map_from_catalog` copies **catalog** rows for that `spec_id` onto the per-map tables. Example (minigames; collectibles is the same shape):

```177:200:src/queries/minigames.rs
/// Copy all catalog rows for a spec onto `minigame_on_map` for this uid.
        INSERT INTO minigame_on_map (map_uid, slug, minigame_key, kind, name, spec_user, name_id)
        SELECT $1, c.slug, ...
        FROM minigame_catalog c
        WHERE c.spec_id = $2
        ON CONFLICT (map_uid, slug) DO UPDATE SET
            ...
            spec_user = EXCLUDED.spec_user,
            name_id = EXCLUDED.name_id
```

Schema (`migrations/20260821101000_minigames.up.sql`, `migrations/20260822163000_collectibles.up.sql`):

- `minigame_catalog` / `collectible_catalog`: spec-side, later given `spec_id` (`migrations/20260821134000_minigame-catalog-spec-id.up.sql`).
- `minigame_on_map` / `collectible_on_map`: `UNIQUE (map_uid, slug)`, columns `spec_user` + `name_id`. **No `spec_id` column. No unique constraint on spec.**

Empty catalog (no `minigames.games[]` / collectible items) inserts 0 rows; `rows_affected() > 0` is discarded by `let _ =`. A floors/triggers/assets-only spec therefore leaves **no** `*_on_map` rows after `ReportMinigameMap`.

There is no hats bind on this master: `src/queries/hats.rs` and `migrations/20260823101500_hats.up.sql` are absent. The plugin still parses hats from aux JSON and can send hat unlocks; that is not a spec↔map table here.

### Secondary binds (same tables, one slug)

- `ReportMinigameAttempt` → `minigames::report_attempt` inserts/updates one `minigame_on_map` row for `(map_uid, slug)` (comment: "insert attempt, bind map, …").
- `ReportCollectibleUnlock` / `ReportCollectibleCollect` → `collectibles::bind_one` for that slug.

These can populate `*_on_map` even if `ReportMinigameMap` never ran, but still only after play.

Anyone connected can report any `spec_id` onto any 27-char UID. There is no author check on `report_minigame_map`.

## 5. Cardinality

- **One spec, many maps.** Bind INSERTs are parameterized by the reported `map_uid`. Catalog uniqueness is `(user_id, name_id, slug)` / `(spec_id, slug)`, not map. Reusing one `url=` on two published maps is a client-only fact until players report; then two `map_uid`s get copies of the same slugs.
- **Many specs, one map.** `UNIQUE (map_uid, slug)`: a later report for the same slug overwrites `spec_user`/`name_id`/`kind`/`name`. Different slugs from different specs stay. The comment format has a single `url=` key, so play-time load is one spec unless the comment is edited or the `assets.xk.io` override differs.
- **`minigame_key` / `collectible_key`** are `md5(map_uid || … || slug)`, so PBs/progress are per map UID, not per spec.

## 6. 24-hour lock is server-side

```1594:1598:src/server2.rs
        let existing = queries::custom_map_aux_specs::get_spec(pool, user_id, name_id).await?;
        if let Some(existing) = existing {
            if existing.created_at < chrono::Utc::now() - chrono::Duration::hours(24) {
                return Err(ApiError::StrErr("Spec can no longer be modified".to_string()));
```

Delete uses the same `created_at + 24h` check (`"Spec can no longer be deleted"`). Clock is **`created_at`**, not `updated_at`. After the window, a new `name_id` is required.

The lock check calls `get_spec`, which also increments `hit_counter`.

Editor copy only (`ProjInfo.as` upload step): *"you can update or delete this JSON (with this Name ID) for 24 hours after uploading it, but it becomes locked after that."* No client-side timer.

## 7. What this means for listing

Issue 2 already treats listing as applying to every registered custom map, not only maps with an aux spec. Height registration is `ReportMapCurrPos` → `map_leaderboard` / `map_curr_heights` (`src/server2.rs` `report_map_curr_pos`; `migrations/20240527032643_shallow-dip-heights.up.sql`). That path never looks at `custom_map_aux_specs`.

If a listing threshold is later stored **inside** an aux spec:

- Upload still does not name a map.
- The only durable spec→map pointer at publish time is the GBX comment `url=` (or the off-server `d++maps/{uid}.txt` comment).
- `*_on_map` is lazy, catalog-only, last-write-wins per slug, and missing for specs with no minigames/collectibles.
- One spec on two UIDs is allowed by every server table today; there is no server object that says "this spec is this map."
