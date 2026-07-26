# Web admin and Trackmania OAuth live in dd2-server

Map Authors need a fast-to-update moderation UI. Embedding a static web admin, TM OAuth (api.trackmania.com authorization code flow), and moderation HTTP API in dd2-server keeps one deploy unit, one database, and authorization next to the Leaderboard filter queries. A separate frontend repo was rejected to avoid dual deploys and split session/auth concerns.

## Runtime configuration

- `TM_OAUTH_CLIENT_ID`, `TM_OAUTH_CLIENT_SECRET`, and `TM_OAUTH_REDIRECT_URI` enable login. The redirect URI must end at `/api/mod/oauth/callback` and match the OAuth client registration.
- `DD2_NADEO_DS_USERNAME` and `DD2_NADEO_DS_PASSWORD` enable lazy Map Author lookup through Nadeo Live Services. `DD2_NADEO_UA_EMAIL` customizes the required contact in the Nadeo user agent.
- OAuth access and refresh tokens are discarded after `/api/user` identifies the player. The admin cookie refers only to a server-side session.
