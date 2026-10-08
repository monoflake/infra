# syntax=docker/dockerfile:1
# cloudflared as the node runs it: the upstream image at a pinned version, run as a
# remotely-managed tunnel. Moving to a new version is this one line. See
# spec/architecture/host.md, "The tunnel is deployed like any app, at the address Caddy trusts".
FROM cloudflare/cloudflared:2026.9.3
# Its token is `TUNNEL_TOKEN`, from the node; its metrics answer host's health check. QUIC unless
# the node's `config.env` names another protocol for `--protocol`, which cloudflared reads from this
# variable; `tunnel_protocol` in nodes.toml writes it.
ENV TUNNEL_TRANSPORT_PROTOCOL=quic
# Which family it reaches Cloudflare over, cloudflared's own default made explicit; a node's
# `tunnel_ip_version` writes another into `config.env`. `auto` takes the family the resolver lists
# first, IPv4 on `edge`, whose IPv6 is a unique local address behind Docker's NAT, and falls back
# from IPv6 to IPv4 only, never the other way; `6` is IPv6 alone.
ENV TUNNEL_EDGE_IP_VERSION=auto
CMD ["tunnel", "--no-autoupdate", "--metrics", "0.0.0.0:20820", "run"]
