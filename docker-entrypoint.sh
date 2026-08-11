#!/bin/sh
# Shared development entrypoint for both dev images.
#
# Named volumes (target/, the cargo registry, node_modules, the bun cache) are
# created root-owned, but the processes inside run as the host user so that
# anything they write into the bind mount stays owned by you. Each mountpoint
# therefore has to be handed over before privileges are dropped, or the first
# write fails with EACCES — cargo cannot create /app/target, bun cannot
# populate node_modules.
#
# Services declare what they need via DEV_CHOWN_PATHS (space-separated).
set -e

for path in ${DEV_CHOWN_PATHS:-}; do
    mkdir -p "$path" 2>/dev/null || true
    [ -e "$path" ] || continue
    # Skip the recursive chown when ownership is already correct — on a
    # populated node_modules or target/ that walk is thousands of files on
    # every container start.
    if [ "$(stat -c '%u' "$path")" != "${DEV_UID}" ]; then
        chown -R "${DEV_UID}:${DEV_GID}" "$path" 2>/dev/null || true
    fi
done

exec gosu "${DEV_UID}:${DEV_GID}" "$@"
