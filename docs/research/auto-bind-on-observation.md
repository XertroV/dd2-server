# Proposal: author-absent binding by reading the published map

Status: **design only, not implemented.** Research context for removing the
"the author must load their own map once" step. Sibling docs:
[aux-spec-map-bind.md](aux-spec-map-bind.md) (how binding works today),
[map-metadata-cache.md](map-metadata-cache.md) (what Nadeo metadata we cache).

## The problem

The authoritative map -> spec binding requires the map's author to be the
reporter of request 81: `prepare_authoritative_bind` checks the reporter owns
the spec and that Nadeo says the reporter authored the map. Until that happens
the map fails closed: no minigame attempts, collectible unlocks, hat unlocks,
and withheld hat reads. The author must load their map once with Dips++
connected, or the features stay off for everyone.

## Why a comment-embedded challenge does not work

A nonce/hash challenge in the map comment proves the comment contains
`H(nonce)` — content possession, not account identity. Comments are copyable:
an attacker can copy a victim's entire comment (including the answered
challenge and the victim's `url=`) into their own map re-upload. The copy gets
a new uid whose Nadeo author is the attacker, and a content check alone cannot
tell the copy from the original. With multiple map versions legitimately
sharing one spec, content-derived identity is ambiguous anyway. The only
non-copyable signal Nadeo exposes is `author` on the map object, so every
secure path has to route through account identity.

## The insight: the published comment is already author-signed

Only the map's author can publish or update a map on Nadeo. Therefore the
`url=` inside a comment **fetched from Nadeo** is an author endorsement of
that spec — equivalent to request 81 from the author, without the author
being online. Proposed verification rule for an author-absent bind:

1. Fetch the published map for a UID from Nadeo.
2. Read its comment; parse the Dips++ `url=` into `(wsid, name_id)`.
3. Resolve the spec row for that `(wsid, name_id)`.
4. Require `spec.user_id == url wsid == Nadeo map author`.
5. Feed the same `commit_authoritative_bind` path as today.

Attack check (comment copy): attacker re-uploads victim's map -> new uid,
Nadeo author = attacker, url wsid = victim -> step 4 mismatch -> refused.
Attack check (wrong-spec lock-in): the spec choice comes from the author's
published comment, not from a client-claimed `spec_id`, so nobody can point a
victim's map at a spec the victim never referenced. (This is exactly why the
cheaper relaxation — letting any reporter's 81 trigger with only
`spec.owner == nadeo author` — is unsafe: `spec_id` is client-supplied and an
attacker could permanently bind a wrong spec owned by the author.)

Multiple map versions sharing one spec are fine: binding is keyed by uid,
which Nadeo preserves across updates. The existing immutable-binding policy
(first verified bind wins, same-spec revisions re-materialize) is unchanged.

## Capability gap (go/no-go probe)

The server cannot read comments today. `LiveMapInfo` (map-info service)
exposes `uid`, `author`, `name`, `thumbnailUrl` only. Options:

- **P1 probe (cheap, do first):** with the existing `DD2_NADEO_DS_*`
  credentials, check whether any map service returns a description/comment
  field, and whether the map download (`.Map.Gbx` by UID) is reachable.
  Dedicated servers must be able to fetch maps, so the DS scope is plausible
  but unverified.
- **P2 fallback:** minimal GBX header parser for the comment chunk (the
  comment lives in the header; no full GBX parse needed). Downloads are
  heavier than map-info (megabytes vs one JSON), so the fetch budget must be
  smaller than the map-author one.
- **Not acceptable as identity sources:** the `assets.xk.io/d++maps/<uid>.txt`
  override (not author-authenticated) and any client-reported comment
  (spoofable).

If both P1 routes fail, this design is blocked; the author-load path remains
the mechanism.

## Trigger, debounce, and budgets

Hook: map adoption on the plugin host (the Wayfinder tracker that already
learns the real uid from 81/213/214 and pings). No background poller — a
lookup is only enqueued when a connection adopts a map that has no verified
binding.

- **Map not on Nadeo yet** (players on a local/unpublished copy): per-uid
  negative backoff starting at ~5 min, growing (5 -> 15 -> 60 min cap). More
  players adopting the map raises the chance it is published, and each new
  adoption retries subject to the per-uid debounce — the user-visible
  behaviour the flow wants.
- **Map exists, no `url=` in comment** (simple map): nothing to bind; long
  backoff (needs an author action to change).
- **Map exists, `url=` wsid != author**: refused (attack or third-party
  re-upload); long backoff.
- **Bound or boundable**: bind, materialize, stop. Also upsert `map_meta`
  from the same fetch (author/name/thumbnail), same as the moderation path.
- **Abuse bounds**: uids in reports are client-supplied; keep a bounded LRU of
  pending/negative uids (e.g. 10k), reuse the admission-limiter pattern from
  `CachedMapAuthorVerifier` with a smaller concurrent budget for downloads,
  and never let the observation path exceed its own hourly cap.

## Resulting flow

Author publishes the map -> first players join (author presence no longer
required) -> first adoption enqueues the fetch -> comment `url=` verified
against the Nadeo author -> binding verified and catalogs materialize. The
author's own request 81 stays as the instant path and bypasses all of this.

## Implementation shape (when approved)

- P1 probe (above) decides the comment source.
- GBX comment-chunk reader with local `.Map.Gbx` fixtures (only if P2).
- Observation hook + per-uid debounce/negative cache in `server2.rs`, reusing
  the spawn/deferred pattern of `report_minigame_map_deferring`.
- A `PreparedBind` variant whose `verified_author` is derived from the
  comment-vs-author match; `commit_authoritative_bind` itself unchanged.
- Tests with fake Nadeo backends (existing `FakeAuthorVerifier` pattern):
  not-on-Nadeo backoff, comment-copy refusal, wrong-wsid refusal, happy-path
  bind, spec-revision re-materialization.
- Deploy: plugin host binary only; no migrations; API untouched.
- After it ships, soften the editor finalization note from "you must play
  your map once" to "activates automatically once your map is live and
  players join; play it yourself to activate instantly".
