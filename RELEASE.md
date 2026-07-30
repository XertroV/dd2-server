# Release / deploy

How we ship `dd2-server` binaries to production, and the footguns that will take the plugin down if ignored.

## Topology

Two hosts, **one shared Postgres**.

| Role | SSH host | Public name | Binary to deploy | Crate entry |
|------|----------|-------------|------------------|------------|
| API / web / admin | `DipsPP-server1` | `dips-plus-plus.xk.io` | `dd2-server` | `src/server.rs` |
| Plugin (TCP + some HTTP) | `dpp-02` | `dips-plus-plus-server.xk.io` | `dd2-server2` | `src/server2.rs` |

On disk both hosts run a process named `./dd2-server` under `/root/plugin-server/` (systemd unit `dd2-server.service` → `run.sh`). The **source binary differs** — never swap them.

```
update-server.sh maps:
  DipsPP-server1  ←  target/release/dd2-server
  dpp-02          ←  target/release/dd2-server2
```

Both binaries embed the same `sqlx::migrate!()` set and run migrations on startup against the **same** `DATABASE_URL`.

## Preferred release path

From a clean checkout / worktree with `.env` linked (for `DATABASE_URL` / sqlx offline if needed):

```bash
# Keep build load light on the workstation
CARGO_BUILD_JOBS=2 ./update-server.sh
```

`update-server.sh` will:

1. `cargo build --release` (both bins)
2. Strip `.note.gnu.property` from both (see footguns)
3. rsync `update.sh`, `logs.sh`, `run.sh`, `migrations/`, and the correct binary to each host as both `dd2-server` and `dd2-server-next`

Then restart **both** services (script does not restart for you):

```bash
ssh DipsPP-server1 'systemctl restart dd2-server && systemctl is-active dd2-server'
ssh dpp-02         'systemctl reset-failed dd2-server; systemctl restart dd2-server && systemctl is-active dd2-server'
```

`run.sh` copies `dd2-server-next` → `dd2-server` on each start if `-next` exists. Prefer leaving a good `-next` in place so a restart always picks the intended build.

### Manual one-host deploy

```bash
CARGO_BUILD_JOBS=2 cargo build --release --bin dd2-server --jobs 2   # API
# or
CARGO_BUILD_JOBS=2 cargo build --release --bin dd2-server2 --jobs 2  # plugin

objcopy --remove-section=.note.gnu.property target/release/dd2-server   # or dd2-server2

rsync -avz target/release/dd2-server  DipsPP-server1:~/plugin-server/dd2-server-next
# or
rsync -avz target/release/dd2-server2 dpp-02:~/plugin-server/dd2-server-next

rsync -avz -r migrations <host>:~/plugin-server/

ssh <host> 'systemctl reset-failed dd2-server; systemctl restart dd2-server'
```

Also rsync helper scripts when they change: `run.sh`, `update.sh`, `logs.sh`.

## Migrations + shared DB (the big footgun)

**Rule: when a release adds or changes sqlx migrations, deploy the new binary to every host that shares the DB before either host is left running an old binary that can restart.**

What goes wrong:

1. API host (`DipsPP-server1`) is deployed with new migrations.
2. On start it applies them and records versions in `_sqlx_migrations` (e.g. `20260726090000`).
3. Plugin host (`dpp-02`) is still on an older binary whose embedded migrator **does not include** those versions.
4. Plugin restarts (crash, OOM, `systemctl restart`, host reboot, deploy of an *old* `-next`) →

   ```
   Error running migrations: VersionMissing(20260726090000)
   ```

   → panic → systemd restart loop → **plugin down**.

Additive migrations do **not** break old *queries*, but they **do** break old binaries at startup because sqlx requires the DB’s applied set ⊆ binary’s known set.

Safe orders:

- **Best:** ship both hosts in one release (`./update-server.sh` + restart both).
- **API-first only if:** you immediately deploy the matching plugin binary, or you accept that any plugin restart before that will brick the plugin.
- **Do not** leave `dd2-server-next` on the plugin host pointing at a pre-migration build after the API has migrated.

Check applied versions:

```bash
ssh DipsPP-server1 'set -a; source ~/plugin-server/.env; set +a; \
  psql "$DATABASE_URL" -c "SELECT version, description, installed_on FROM _sqlx_migrations ORDER BY version;"'
```

## Other footguns

### Wrong binary on the wrong host

Deploying `dd2-server2` to the API host (or `dd2-server` to the plugin host) “works” as a process name but serves the wrong stack. Always match the table above. Logs should show crate `dd2_server` vs `dd2_server2`.

### `.note.gnu.property` / older CPUs

aws-lc (via rustls) can stamp an ELF property that makes the dynamic linker refuse the binary on older CPUs. Always run:

```bash
objcopy --remove-section=.note.gnu.property target/release/dd2-server
objcopy --remove-section=.note.gnu.property target/release/dd2-server2
```

(`update-server.sh` already does this.)

### Overwriting a running binary

rsync onto the live `dd2-server` inode while the process is running can be messy. Prefer writing `dd2-server-next` and `systemctl restart` so `run.sh` does the swap. `update-server.sh` currently writes both; restart promptly after sync.

### Env is per-host

Do not blindly clobber remote `.env`. In particular:

- Preserve `DATABASE_URL` on both hosts (shared DB).
- OAuth / Nadeo (`TM_OAUTH_*`, `DD2_NADEO_DS_*`) are required on the **API** host for `/admin` and map-author lazy fetch.
- Plugin host may log `DD2_NADEO_DS_USERNAME/PASSWORD not set` — expected if you only configured Nadeo on the API host; plugin still serves game traffic.

### systemd restart storms

After a migration panic, systemd may hit `Start request repeated too quickly` and stay failed. Clear before retrying:

```bash
ssh dpp-02 'systemctl reset-failed dd2-server; systemctl restart dd2-server'
```

### Admin HTML is compile-time

`web/admin.html` is `include_str!`’d into the API binary. UI-only changes still need a rebuild + API host redeploy (hard refresh client-side).

### Build load

Prefer `--jobs 2` / `CARGO_BUILD_JOBS=2` on shared workstations so cargo doesn’t melt the box.

## Smoke checks

After any release:

```bash
# API host
curl -sS -o /dev/null -w 'admin:%{http_code}\n' https://dips-plus-plus.xk.io/admin
ssh DipsPP-server1 'systemctl is-active dd2-server; journalctl -u dd2-server -n 20 --no-pager'

# Plugin host
curl -sk -o /dev/null -w 'live_global:%{http_code}\n' https://dips-plus-plus-server.xk.io/live_heights/global
ssh dpp-02 'systemctl is-active dd2-server; journalctl -u dd2-server -n 30 --no-pager'

# Both: no VersionMissing / panic since restart
ssh dpp-02 'journalctl -u dd2-server --since "5 min ago" --no-pager | grep -iE "panic|VersionMissing" || echo OK'
```

Healthy plugin logs show `Migrations ran successfully`, listen on `:17677` and `:443`, and periodic `authenticated` / `New player connected` lines. Socket `UnexpectedEof` on disconnect and occasional `rustls_acme` TLS probe noise are normal.

Empty `/live_heights/global` only means nobody is currently live on the default DD2 UID — check a map that has recent `map_curr_heights` rows if you need a positive live-heights signal.

## Rollback

1. Keep a previous known-good binary on the host (e.g. `dd2-server-old`) before overwriting `-next`.
2. Point `-next` at the old binary (or rsync it back), restart.
3. **Do not** roll back to a binary that lacks migrations already applied in `_sqlx_migrations` — you will hit `VersionMissing` again. Rollback of schema requires an explicit down-migration plan; binary rollback alone only works if the old binary still embeds every applied version.

## Checklist (migration-bearing release)

- [ ] Build both bins from the same commit
- [ ] `objcopy` both
- [ ] Sync binaries + `migrations/` to **both** hosts
- [ ] Restart API host; confirm migrations OK
- [ ] Restart plugin host immediately after (or before leaving the desk)
- [ ] Smoke API `/admin` + plugin live heights / auth traffic
- [ ] Confirm no `VersionMissing` in either journal
