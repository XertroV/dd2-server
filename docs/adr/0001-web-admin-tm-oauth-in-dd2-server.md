# Web admin and Trackmania OAuth live in dd2-server

Map Authors need a fast-to-update moderation UI. Embedding a static web admin, TM OAuth (api.trackmania.com authorization code flow), and moderation HTTP API in dd2-server keeps one deploy unit, one database, and authorization next to the Leaderboard filter queries. A separate frontend repo was rejected to avoid dual deploys and split session/auth concerns.
