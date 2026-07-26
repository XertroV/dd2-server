# Dips++ Platform

Height-climbing Maps (Deep Dip–style), with leaderboards, live presence, and author-led self-moderation. Deep Dip 2 is the founding event; the durable product is any Dips++-enabled Map.

## Language

### Maps and standing

**Map**:
A Dips++-enabled climb map identified by its Trackmania map UID.
_Avoid_: Custom map (except when contrasting legacy DD2 special-cases), track, course

**Standing**:
A player's recorded best height (and related live presence) on a Map.
_Avoid_: Run (this system does not store per-attempt history), race, time attack result

**Leaderboard**:
The ordered public list of Standings on a Map (or the global DD2 height list), excluding players who must not appear.

### Roles

**Map Author**:
The Nadeo map author for a Map's UID (account WSID from Nadeo map metadata). Not the uploader/submitter.
_Avoid_: Uploader, submitter, owner (unless meaning Map Author), mapper (ambiguous)

**Map Delegate**:
A player granted map-local moderation power on a Map by the Map Author or a Super-mod. Equal day-to-day moderation powers to the Map Author on that Map.
_Avoid_: Admin (ambiguous with Super-mod), moderator (unless Map Delegate), co-author

**Super-mod**:
A platform-level operator who can moderate any Map, manage Map Delegates, and manage other Super-mods.
_Avoid_: Admin (prefer Super-mod), ops (informal)

### Moderation

**Map Ban**:
An explicit, Map-scoped exclusion of a player from that Map's public Leaderboard and live presence. Height updates still record. Distinct from Shadowban.
_Avoid_: Shadowban, hide (except in UI copy), mute, kick

**Unofficial**:
A `(player, Map)` mark that the player's Standing on that Map does not count as official for competition/prize interpretation. Set automatically when a Map Ban is applied; may remain after a ban ends depending on how the ban ended.
_Avoid_: Invalid, cheated, DQ (those imply stronger claims)

**Shadowban**:
A platform-global, silent exclusion from Leaderboards and live presence. Operator-only; not author-configurable.
_Avoid_: Map Ban, ban (unqualified)

**Private reason**:
Moderator-only audit text attached to a Map Ban.
_Avoid_: Reason (unqualified when both fields exist)

**Public message**:
Text shown to a Map-Banned player explaining the ban.
_Avoid_: Reason (unqualified when both fields exist)
