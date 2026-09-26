# Deploying Lumina on a server

Runs the Lumina Discord bot with its **embedded daemon** in Docker, behind your
existing **host nginx**. You configure everything from a **browser wizard** on
first run; after that, the admin surface is unreachable from the internet.

## The flow

```bash
# 1. one-time: point nginx at the app (allowlists /setup + /auth/mcp only)
sudo cp deploy/nginx-lumina.conf /etc/nginx/sites-available/lumina   # edit the domain first
sudo ln -s /etc/nginx/sites-available/lumina /etc/nginx/sites-enabled/lumina
sudo nginx -t && sudo systemctl reload nginx

# 2. launch — prints a setup link
cd deploy
./start.sh lumina.simplychris.ai
```

`start.sh` builds + starts the container and prints a one-time link like
`https://lumina.simplychris.ai/setup?token=…`. Open it, fill in the wizard
(LLM key, your email, Discord token + **Invite the bot** button, Google OAuth
client), and hit **Finish**. The container restarts itself already configured —
the bot comes online and the `/setup` page **404s forever**.

## Why this is safe

The daemon trusts any loopback connection as **admin**, and only ever binds
`127.0.0.1:9800` (the host's loopback, via `network_mode: host`). Host nginx
allowlists exactly two things and 404s everything else:

- `/auth/mcp/*` — the Google OAuth callback (so Discord users can import docs);
- `/setup` — the first-run wizard, itself gated by a one-time token that the
  daemon stops honoring the moment setup completes.

`/admin`, `/admin/api`, `/api`, `/ws` are never proxied. That allowlist is the
**entire security boundary** — don't add a catch-all `location /` proxy.

```
Internet ──443──▶ host nginx ──/setup, /auth/mcp/*──▶ 127.0.0.1:9800 (daemon)
                      └──────────── everything else ──▶ 404
```

> Anyone with a shell on the host can still reach `127.0.0.1:9800` as admin
> (inherent to the loopback-trust model) — fine for a single-admin box.

## Data & config live on the host

Bind-mounted to `./data/` (NOT a Docker volume), so it survives rebuilds and is
editable over SSH. The wizard writes the `config/*.toml` files for you:

| Host path                          | What                                       |
| ---------------------------------- | ------------------------------------------ |
| `data/config/settings.toml`        | public_url, daemon_secret, API keys, model |
| `data/config/lumina.toml`          | Discord bot token, owner / guild ids       |
| `data/config/oauth_providers.toml` | Google OAuth client id / secret            |
| `data/database/noema.db`           | SQLite (sessions, entities, content)       |
| `data/blob_storage/`               | content-addressed (CAS) blob store         |
| `data/vault/`                      | Markdown vault files                       |
| `data/logs/lumina.log`             | app logs (daily-rolled; also in `docker compose logs`) |
| `data/models/voices/`              | custom pocket-tts voices (`<name>.safetensors` or `<name>.wav`), offered by `/voice set-voice` as `<name>` |
| `data/hf-cache/`                   | Hugging Face model cache (pocket-tts weights; `token` for gated repos) |
| `data/SETUP-URL.txt`               | the current setup link (deleted on setup)  |

## Prerequisites

- Linux server with Docker + Docker Compose (`network_mode: host` is Linux-only).
- Your existing **nginx + Certbot** cert for the domain.
- A **Discord bot** (token + Application ID from the Developer Portal).
- A **Google OAuth 2.0 Web client** (Cloud Console > Credentials). The wizard
  shows you the exact Authorized redirect URI to paste:
  `https://<domain>/auth/mcp/callback`.

## Reconfiguring later

SSH in, edit the TOMLs, restart:

```bash
vim data/config/settings.toml      # e.g. add an api key / change model
docker compose restart lumina
```

To re-run the setup wizard from scratch, use the helper — it clears the owner
email, restarts, and prints a fresh setup link:

```bash
./reset-setup.sh
```

Logs are both in `docker compose logs -f lumina` and on disk at
`data/logs/lumina.log` (daily-rolled).

## How a Discord user imports Google Docs

1. The bot hands them `https://<domain>/auth/mcp/google?external_id=discord:<id>`.
2. Google redirects to `https://<domain>/auth/mcp/callback`.
3. nginx proxies that route to the daemon, which stores their token. Done.

## Git-push deploy (auto-deploy on push)

`deploy/post-receive` is a hook for the bare deploy repo
(`~/projects/lumina.git`) that, on every push to `main`, checks the code out
into `~/apps/lumina`, rebuilds the image, and rolling-restarts the container.
Persistent `deploy/data/` (config, DB, blobs) is untracked and survives.

Install it once on the server:

```bash
git --git-dir=~/projects/lumina.git show main:deploy/post-receive \
    > ~/projects/lumina.git/hooks/post-receive
chmod +x ~/projects/lumina.git/hooks/post-receive
```

After that, from your laptop:

```bash
git push deploy HEAD:refs/heads/main     # builds + restarts on the server
```

First-time only: run `./start.sh <domain>` on the server once to seed
`public_url` and get the setup link; later pushes just rebuild + restart.
(The Rust build means the first push blocks a few minutes while it compiles.)

## Notes

- Admin login via Google (so the admin pages can be opened over the internet as
  the owner) is **planned** — the wizard shows a "coming soon" marker for it.
- The daemon port (default `9800`) is `daemon_port` in `settings.toml`; if you
  change it, update `proxy_pass` in `nginx-lumina.conf` too.
- The image builds only the `lumina` binary; the wizard is embedded, so there's
  no Node / admin-UI build. The admin SPA is intentionally not shipped.
