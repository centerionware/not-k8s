FROM ubuntu:24.04

ENV container=docker
STOPSIGNAL SIGRTMIN+3

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        bpftool \
        clang \
        conntrack \
        containerd \
        ethtool \
        iproute2 \
        iptables \
        iputils-ping \
        llvm \
        systemd \
        systemd-sysv \
        util-linux \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/* \
    && systemctl enable containerd

COPY systemd-entrypoint.sh /usr/local/sbin/nodemigrate-systemd-entrypoint
RUN chmod 0755 /usr/local/sbin/nodemigrate-systemd-entrypoint

ENTRYPOINT ["/usr/local/sbin/nodemigrate-systemd-entrypoint"]
