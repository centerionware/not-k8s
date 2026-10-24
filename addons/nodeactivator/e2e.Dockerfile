# syntax=docker/dockerfile:1
FROM scratch
COPY --chmod=0755 dist-bin/nodeactivator /nodeactivator
ENTRYPOINT ["/nodeactivator"]
