# Per-user execution sandbox for Skald-Circle (blueprint §6).
#
# Our own image — not a public one — so we can install exactly what the runtime
# needs over time without depending on an external base that could change or go
# away. Built once at boot by `ContainerManager::ensure_image` (tag `skald-runtime`).
#
# Holds python + node so `execute_cmd` (and, later, per-user MCP servers) run
# inside the user's container against their bind-mounted home.
#
# What belongs here vs. what an agent installs on demand: `sudo apt-get install`
# works inside the sandbox, but it re-downloads on **every** container recreate,
# inside a task, where it costs latency and can fail. Preinstalling costs image
# size **once for the whole box** — there is one image, shared by every user's
# container — so anything an agent reaches for repeatedly is cheaper baked in.
# What is deliberately left out is the converse: `build-essential`/`python3-dev`
# (~270 MB, only for `pip install` of a package with no wheel) and `pandoc`
# (~216 MB, niche) are big *and* self-recoverable, so they stay on demand.

FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    # Language runtimes.
        python3 \
        python3-pip \
        python3-venv \
        nodejs \
        npm \
    # Base plumbing. `util-linux` provides `setsid` (see the sudoers note below).
        ca-certificates \
        curl \
        wget \
        git \
        openssh-client \
        sudo \
        util-linux \
        procps \
        less \
        file \
        tzdata \
        dnsutils \
        iputils-ping \
    # Shell-work staples: JSON, fast search, archives, local data.
        jq \
        ripgrep \
        unzip \
        zip \
        xz-utils \
        sqlite3 \
    # Media + documents. `ffmpeg` brings `ffprobe`; `poppler-utils` brings
    # `pdftotext`. The tesseract language packs match the app's supported UI
    # locales (`i18n::SUPPORTED_LOCALES`) — `eng` and `osd` arrive as hard deps.
        ffmpeg \
        imagemagick \
        poppler-utils \
        tesseract-ocr \
        tesseract-ocr-ita \
        tesseract-ocr-fra \
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
