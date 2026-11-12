# Stable default. Included only inside the `xxgate_active` upstream block.
# Keep the candidate on another loopback port (for example
# 8798) until it passes /readyz, then switch this file and reload Nginx.
server 127.0.0.1:8797;
