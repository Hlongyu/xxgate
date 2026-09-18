# Deployment

- Do not compile XXGate on production servers, including inside containers.
- Build and test the Linux amd64 release image locally, then export and transfer
  the completed image with a SHA-256 checksum.
- Production hosts may verify checksums, load the image, run candidate checks,
  back up data, and activate the release with `--no-build`.
- Preserve production credentials, accounts, keys, request history, and data volumes.

# Code discovery

Prefer codebase-memory MCP graph tools for symbols and call relationships. Use
direct source searches for literals/configuration or when graph coverage is stale
or insufficient.
