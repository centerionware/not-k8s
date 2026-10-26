FROM rust:bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p nodeactivator

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates openssl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home nodeactivator
COPY --from=build /src/target/release/nodeactivator /usr/local/bin/nodeactivator
COPY --from=build /src/target/release/nodeactivator-cert-signer /usr/local/bin/nodeactivator-cert-signer
USER 10001:10001
EXPOSE 8080 8443
ENTRYPOINT ["/usr/local/bin/nodeactivator"]
