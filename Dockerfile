FROM rust:1.95-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

RUN CARGO_INCREMENTAL=0 cargo build --release \
    -p bcp-controller \
    -p bcp-agent \
    -p bcp-client \
    -p bcp-e2e

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/bcp-controller /usr/local/bin/bcp-controller
COPY --from=builder /src/target/release/bcp-agent /usr/local/bin/bcp-agent
COPY --from=builder /src/target/release/bcp-client /usr/local/bin/bcp
COPY --from=builder /src/target/release/bcp-e2e /usr/local/bin/bcp-e2e

ENTRYPOINT ["/usr/local/bin/bcp"]
