# Proposal: author-absent binding by reading the published map

Status: **design + probe results, not implemented.** Research context for
removing the "the author must load their own map once" step. Sibling docs:
[aux-spec-map-bind.md](aux-spec-map-bind.md) (how binding works today),
[map-metadata-cache.md](map-metadata-cache.md) (what Nadeo metadata we cache).

## The problem

The authoritative map -> spec binding requires the map's author to be the
reporter of request 81: `prepare_authoritative_bind` checks the reporter owns
the spec and that Nadeo says the reporter authored the map. Until that happens
the map fails closed: no minigame attempts, collectible unlocks, hat unlocks,
and withheld hat reads. The author must load their map once with Dips++
connected, or the features stay off for everyone.

This also **blocks team maps today**: if a team member uploads the spec and
the leader publishes the map (the Deep Dip / Shallow Dip pattern), the
leader's own load fails `SpecNotOwned` (reporter does not own the teammate's
spec) and the teammate's load fails `AuthorMismatch` (teammate is not the
Nadeo author). No one in the team can bind at all.

## Why a comment-embedded challenge does not work

A nonce/hash challenge in the map comment proves the comment contains
`H(nonce)` — content possession, not account identity. Comments are copyable:
an attacker can copy a victim's entire comment (including the answered
challenge and the victim's `url=`) into their own map re-upload. The copy gets
a new uid whose Nadeo author is the attacker, and a content check alone cannot
tell the copy from the original. With multiple map versions legitimately
sharing one spec, content-derived identity is ambiguous anyway.

## The endorsement model (revised: no ownership equality)

Only the map's publishing account can publish or update a map on Nadeo.
Therefore the `url=` inside a comment **fetched from Nadeo by the server** is
the publisher's endorsement of that spec — whoever owns it. Proposed rule for
an author-absent bind:

1. Fetch the published map for a UID from Nadeo (server-side only).
2. Read its comment; parse the Dips++ `url=` into `(wsid, name_id)`.
3. Resolve the spec row for exactly that `(wsid, name_id)` — the same lookup
   any client's `GET /aux_spec/<wsid>/<name_id>.json` performs.
4. Bind via the existing `commit_authoritative_bind` path.

There is **no** `spec.user_id == Nadeo author` requirement: team maps where
the uploader, the map author, and the publisher are different accounts are the
normal case this must support. (`map-info` exposes `author` and `submitter`
separately, so distinctions remain observable for moderation if ever needed.)

Attack analysis without the equality:

- **Bind a spec the publisher never endorsed**: requires the published
  comment to reference it; only the publishing account can change that.
  Comment copies re-uploaded by an attacker produce a *new uid* bound to the
  attacker's own map — identical to what today's 81 path already allows for
  re-uploads, so no new capability.
- **Wrong-spec lock-in**: if a spec owner manages to get their `url=` embedded
  by the publisher (contributor/social engineering), the immutable
  first-bind-wins policy would lock it in. Mitigations to decide at impl
  time: allow a comment-derived bind to follow the *latest* comment (the
  publisher can always correct by re-publishing), or keep immutability and
  add an admin unbind. This is a policy choice, not a security hole: only the
  publishing account can put any url in the comment at all.
- Client-reported comments (context reports) remain untrusted; only the
  Nadeo-downloaded file is an endorsement.

## Probe results (2026-09-08, scratch scripts in `dips++-all/.scratch/nadeo-map-probe/`)

GO — everything needed exists today with the current `DD2_NADEO_DS_*` creds:

- `GET live-services.../api/token/map/<uid>` returns **`author` AND
  `submitter`** (both UUIDs), name, uid, `downloadUrl`, `thumbnailUrl`,
  upload/update timestamps. No comment field.
- `downloadUrl` (`core.trackmania.nadeo.live/maps/<mapId>/file`) is
  **unauthenticated**, returns the full `.Map.Gbx`, and **honours `Range`
  requests** (verified `206`, `Accept-Ranges: bytes`). DD2 = 5,998,704 B.
- **The map comment is in the GBX header, uncompressed**: header chunk
  `0x03043007` stores `<Thumbnail.jpg>` (JPEG, EOI `FFD9`) followed by
  `<Comments>` + length u32(s) + plain text. Verified on a Nadeo-downloaded
  file (DD2: "Please do not create unofficial versions…") and on a local
  Dips++ test map (full `--BEGIN-DPP--\nurl=https://…/aux_spec/<wsid>/<name_id>.json`
  readable verbatim). The LZ-compressed copy in the body is irrelevant.
- The chunk table is in the first ~64 bytes (`count` at 0x11 for both
  samples; entries are `u32 id`, `u32 size|0x80000000 flag`). The comment
  therefore sits at a computable offset: chunk-table read (1 range request,
  ~128 B) -> thumbnail chunk start+size -> one more range request for the
  last few KB of that chunk -> find `<Comments>`. **~4 KB total transfer for
  any map size**; a v1 can simply download the whole file (0.4–6 MB) since
  lookups are debounced and rare.

## Trigger, debounce, and budgets

Hook: map adoption on the plugin host (the Wayfinder tracker that already
learns the real uid from 81/213/214 and pings). No background poller — a
lookup is only enqueued when a connection adopts a map that has no verified
binding.

- **Map not on Nadeo yet** (players on a local/unpublished copy): per-uid
  negative backoff starting at ~5 min, growing (5 -> 15 -> 60 min cap). More
  players adopting the map raises the chance it is published, and each new
  adoption retries subject to the per-uid debounce.
- **Map exists, no `url=` in comment** (simple map): nothing to bind; long
  backoff (needs an author action to change).
- **Map exists, `url=` resolves to a spec**: bind, materialize, stop. Also
  upsert `map_meta` from the same `map-info` call (author/name/thumbnail).
- **`url=` does not resolve** (spec deleted / wsid unknown): long backoff.
- **Abuse bounds**: uids in reports are client-supplied; keep a bounded LRU
  of pending/negative uids (e.g. 10k), reuse the admission-limiter pattern
  from `CachedMapAuthorVerifier` with a small concurrent budget; map-info is
  one cheap JSON call, the file fetch is range-limited as above.

## Resulting flow

Author (or team) publishes the map -> first players join (author presence not
required) -> first adoption enqueues the fetch -> comment `url=` resolves ->
binding verified and catalogs materialize. The author's own request 81 stays
as the instant path and bypasses all of this — but note 81 should eventually
be relaxed the same way (spec owner need not be the reporter) or team maps
will still need the observation path.

## Implementation shape (when approved)

- `nadeo.rs`: `map_info` extended to keep `submitter`/`downloadUrl`;
  a `fetch_map_comment(map_uid)` doing the staged range reads + chunk-table
  walk + `<Comments>` parse (plain text; tolerate the double length u32 seen
  in lookback strings). No GBX body parsing, no decompressor needed.
- Observation hook + per-uid debounce/negative cache in `server2.rs`, reusing
  the deferred-spawn pattern.
- A `PreparedBind` variant whose `verified_author` is None-with-comment-source
  (ownership equality removed); `commit_authoritative_bind` unchanged except
  the verification derivation; decide the rebind policy (follow-comment vs
  immutable + admin unbind).
- Tests with fake Nadeo backends (existing pattern): not-on-Nadeo backoff,
  comment-copy refusal (new-uid case), no-url backoff, happy-path bind for a
  team map (uploader != author), spec-revision re-materialization.
- Deploy: plugin host binary only; no migrations; API untouched.
- After it ships, soften the editor finalization note to "activates
  automatically once your map is live and players join".
