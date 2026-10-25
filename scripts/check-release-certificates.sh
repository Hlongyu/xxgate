#!/bin/sh
# Run inside the completed release image before exporting it. This checks the
# system trust store; embedded Rust trust stores are pinned separately in Cargo.lock.
set -eu

. /etc/os-release
if [ "${ID:-}" != debian ] || [ "${VERSION_CODENAME:-}" != bookworm ]; then
    echo "Unsupported release base: update the certificate check for this distribution." >&2
    exit 1
fi

# Debian's upstream fix for the October 2026 eMudhra incident.
minimum_ca_version='20250419~deb12u2'
installed_ca_version=$(dpkg-query -W -f='${Version}' ca-certificates)
if ! dpkg --compare-versions "$installed_ca_version" ge "$minimum_ca_version"; then
    echo "ca-certificates $installed_ca_version is older than required $minimum_ca_version; rebuild with updated packages." >&2
    exit 1
fi
if [ ! -s /etc/ssl/certs/ca-certificates.crt ] || [ ! -r /etc/ssl/certs/ca-certificates.crt ]; then
    echo "The system CA bundle is missing, empty, or unreadable." >&2
    exit 1
fi
echo "System CA package check passed: ca-certificates $installed_ca_version (Debian Bookworm)."
