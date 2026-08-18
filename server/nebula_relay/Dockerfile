# Multi-stage build for nebula_relay on Linux (x86_64).
#
#   docker build -t nebula-relay -f server/nebula_relay/Dockerfile .
#   docker run -d --name nebula-relay -p 7100:7100/udp nebula-relay
#
# The relay speaks QUIC over UDP, so the published port MUST be /udp.

# ---- build stage ----------------------------------------------------------
FROM ubuntu:22.04 AS build

ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl gnupg cmake g++ ninja-build openssl libcurl4-openssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Install libmsquic from Microsoft's package repository.
RUN curl -sSL https://packages.microsoft.com/keys/microsoft.asc \
        | gpg --dearmor -o /usr/share/keyrings/microsoft.gpg \
    && echo "deb [signed-by=/usr/share/keyrings/microsoft.gpg] https://packages.microsoft.com/ubuntu/22.04/prod jammy main" \
        > /etc/apt/sources.list.d/microsoft.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends libmsquic \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
# Bring in just what the relay needs: its source + the shared protocol header.
COPY server/nebula_relay/main.cpp                server/nebula_relay/main.cpp
COPY server/nebula_relay/CMakeLists.standalone.txt server/nebula_relay/CMakeLists.txt
COPY core/inc/RelayProtocol.h                  core/inc/RelayProtocol.h

RUN cmake -S server/nebula_relay -B build -G Ninja -DCMAKE_BUILD_TYPE=Release \
    && cmake --build build

# Generate a self-signed cert at build time (replace with a real one in prod).
RUN openssl req -x509 -newkey rsa:2048 -nodes \
        -keyout /relay_key.pem -out /relay_cert.pem -days 3650 \
        -subj "/CN=nebula-relay"

# ---- runtime stage --------------------------------------------------------
FROM ubuntu:22.04 AS runtime

ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl gnupg libcurl4 \
    && curl -sSL https://packages.microsoft.com/keys/microsoft.asc \
        | gpg --dearmor -o /usr/share/keyrings/microsoft.gpg \
    && echo "deb [signed-by=/usr/share/keyrings/microsoft.gpg] https://packages.microsoft.com/ubuntu/22.04/prod jammy main" \
        > /etc/apt/sources.list.d/microsoft.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends libmsquic \
    && apt-get purge -y curl gnupg && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/build/nebula_relay /usr/local/bin/nebula_relay
COPY --from=build /relay_cert.pem /etc/nebula-relay/relay_cert.pem
COPY --from=build /relay_key.pem  /etc/nebula-relay/relay_key.pem

EXPOSE 7100/udp
ENTRYPOINT ["/usr/local/bin/nebula_relay", \
            "--port", "7100", \
            "--cert", "/etc/nebula-relay/relay_cert.pem", \
            "--key",  "/etc/nebula-relay/relay_key.pem"]
