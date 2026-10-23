FROM scratch
COPY dist-bin/nodeactivator /nodeactivator
ENTRYPOINT ["/nodeactivator"]
