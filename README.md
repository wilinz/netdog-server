# netdog-server

Alert server for [netdog](https://github.com/wilinz/netdog) agents.

Each agent keeps a **WebSocket connection** open and sends periodic heartbeats.
This server is the **dead-man switch**: if a device stops sending heartbeats
(it went fully offline / lost power / lost uplink) the server notices and emails
you — covering exactly the case where the device itself *can't* send the alert.
It also emails when an agent reports its proxy is unhealthy while still online.

Run it somewhere with reliable connectivity (ideally the mail host itself).

## Endpoints

- `GET /agent` — WebSocket endpoint for agents (`ws://host:8688/agent?token=...&device=...`)
- `GET /` — HTML status page (auto-refreshing)
- `GET /api/status` — JSON device list
- `GET /healthz` — liveness probe

## Alerts

- 🟢 **online** — an agent (re)connects and sends its first heartbeat
- ⚠️ **unhealthy** — agent online but reports its proxy probe is failing
- ✅ **recovered** — proxy healthy again
- 🔴 **offline** — no heartbeat for `NETDOG_OFFLINE_TIMEOUT` seconds (dead-man)

## Configuration (environment variables)

| Variable | Default | Notes |
|---|---|---|
| `NETDOG_LISTEN` | `0.0.0.0:8688` | bind address |
| `NETDOG_TOKEN` | *(empty)* | shared secret; agents must match. **Set this.** |
| `NETDOG_OFFLINE_TIMEOUT` | `90` | seconds without heartbeat → offline |
| `NETDOG_EMAIL_ENABLED` | `true` | |
| `NETDOG_EMAIL_TO` | | comma-separated recipients |
| `SMTP_HOST` / `SMTP_PORT` | / `465` | |
| `SMTP_USE_SSL` | `true` | implicit TLS (port 465) |
| `SMTP_USERNAME` / `SMTP_PASSWORD` | | |
| `SMTP_FROM` / `SMTP_DISPLAY_NAME` | | |

## Run with Docker

```sh
cp .env.example .env     # then edit .env (token + SMTP)
docker compose up -d --build
```

The status page is then at `http://<host>:8688/`.

### TLS (wss)

For `wss://` put a reverse proxy (Caddy/nginx) in front terminating TLS and
proxying to this container's port 8688. Then point agents at `wss://host/agent`.

## Agent side

On the router (netdog), enable the heartbeat:

```sh
uci set netdog.server.enabled='1'
uci set netdog.server.url='ws://your-server:8688/agent'
uci set netdog.server.token='<same as NETDOG_TOKEN>'
uci set netdog.server.device='home-router'
uci commit netdog && /etc/init.d/netdog reload
```

## License

MIT
