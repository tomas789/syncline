# 🛡️ Running Behind Nginx (WSS)

If you are exposing Syncline to the wild internet (and not just your local 10.0.0.0/8 LAN tailscale network), you absolutely **must** use TLS (Secure WebSockets). Why? Because your data syncs over plain HTTP connections if you stick to `ws://`!

The server right now doesn't handle SSL/TLS termination natively, so you'll want to deploy a reverse proxy like Nginx in front of it to do the heavy lifting of encryption.

Here is a quick config snippet to get Nginx proxies working perfectly with Syncline WebSockets:

```nginx
server {
    listen 443 ssl;
    server_name sync.yourdomain.com; # Ensure your DNS A-Record points here

    # SSL Certs (Let's Encrypt / Certbot is great for this)
    ssl_certificate /etc/letsencrypt/live/sync.yourdomain.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/sync.yourdomain.com/privkey.pem;

    location /sync {
        proxy_pass http://127.0.0.1:3030/sync; # Our server

        # WEBSOCKET MAGIC HAPPENS HERE
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";

        # Keepalives
        proxy_read_timeout 86400;
        proxy_send_timeout 86400;

        # Standard Headers
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    }
}
```

Once Nginx is up and running, you just point your local or Obsidian clients to your brand-spanking-new WSS address: `wss://sync.yourdomain.com/sync` and enjoy the secure vibes! 🔒
