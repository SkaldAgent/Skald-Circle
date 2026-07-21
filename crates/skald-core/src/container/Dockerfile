# Per-user execution sandbox for Skald-Circle (blueprint §6).
#
# Our own image — not a public one — so we can install exactly what the runtime
# needs over time without depending on an external base that could change or go
# away. Built once at boot by `ContainerManager::ensure_image` (tag `skald-runtime`).
#
# Holds python + node so `execute_cmd` (and, later, per-user MCP servers) run
# inside the user's container against their bind-mounted home. Kept minimal;
# grow it here as needs arise.

FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 \
        python3-pip \
        python3-venv \
        nodejs \
        npm \
        ca-certificates \
        curl \
        git \
        sudo \
        util-linux \
    && rm -rf /var/lib/apt/lists/*

# The container runs as the host process's uid:gid (blueprint §6 UID coherence), so
# in-container work and the host fs-tools share ownership on the bind mounts. That
# user is not root, so a blanket passwordless sudo restores install capability
# (`sudo apt-get install …`, `sudo npm i -g …`) inside the user's own sandbox — no
# security boundary is crossed (the isolation is the mount set, not the uid; the
# container was already full-root before). `util-linux` provides `setsid`, used to
# make `execute_cmd` killable as a process group.
RUN echo 'ALL ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/skald-nopasswd \
    && chmod 0440 /etc/sudoers.d/skald-nopasswd

WORKDIR /root

# The container is long-lived: created once, started at boot, exec'd into per
# command. Nothing runs until `docker exec` drives it.
CMD ["sleep", "infinity"]
