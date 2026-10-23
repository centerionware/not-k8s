# syntax=docker/dockerfile:1
FROM scratch
COPY --chmod=0755 dist-bin/nodeactivator /usr/local/bin/nodeactivator
COPY --chmod=0755 dist-bin/nodeactivator-cert-signer /usr/local/bin/nodeactivator-cert-signer
