# syntax=docker/dockerfile:1
FROM alpine:3.22
RUN apk add --no-cache ca-certificates openssl
COPY --chmod=0755 dist-bin/nodeactivator-cert-signer /nodeactivator-cert-signer
ENTRYPOINT ["/nodeactivator-cert-signer"]
