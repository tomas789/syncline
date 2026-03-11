# Running Behind Nginx (WSS)

If you're exposing Syncline to the internet (not just your LAN or Tailscale network), you need TLS. Without it, your vault data travels in plaintext over `ws://`.

The server doesn't handle TLS termination itself, so put a reverse proxy in front of it. Here's a working Nginx config:

```nginx
server {
    listen 443 ssl;
    server_name sync.yourdomain.com; # Point your DNS A-record here

    # SSL certs — Let's Encrypt / Certbot works well
    ssl_certificate /etc/letsencrypt/live/sync.yourdomain.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/sync.yourdomain.com/privkey.pem;

    location /sync {
        proxy_pass http://127.0.0.1:3030/sync;

        # WebSocket upgrade headers
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";

        # Long-lived connections — WebSockets stay open
        proxy_read_timeout 86400;
        proxy_send_timeout 86400;

        # Standard headers
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    }
}
```

Once that's up, point your Obsidian clients to `wss://sync.yourdomain.com/sync`.
