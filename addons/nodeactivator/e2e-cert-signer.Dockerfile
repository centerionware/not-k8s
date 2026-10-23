# syntax=docker/dockerfile:1
FROM alpine:3.22
RUN apk add --no-cache ca-certificates gdb openssl
COPY --chmod=0755 dist-bin/nodeactivator-cert-signer /nodeactivator-cert-signer
ENTRYPOINT ["gdb", "--batch", "-ex", "run", "-ex", "bt", "--args", "/nodeactivator-cert-signer"]
