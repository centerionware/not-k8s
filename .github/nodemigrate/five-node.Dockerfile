FROM ubuntu:24.04

ENV container=docker
STOPSIGNAL SIGRTMIN+3

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        clang \
        containernetworking-plugins \
        conntrack \
        containerd \
        ethtool \
        iproute2 \
        iptables \
        iputils-ping \
        llvm \
        linux-tools-common \
        linux-tools-generic \
        systemd \
        systemd-sysv \
        util-linux \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /opt/cni/bin \
    && cp -a /usr/lib/cni/. /opt/cni/bin/ \
    && bpftool_path="$(find /usr/lib -type f -path '/usr/lib/linux-tools-*/bpftool' -print -quit)" \
    && test -n "$bpftool_path" \
    && ln -sf "$bpftool_path" /usr/local/bin/bpftool \
    && command -v bpftool \
    && systemctl enable containerd

COPY systemd-entrypoint.sh /usr/local/sbin/nodemigrate-systemd-entrypoint
RUN chmod 0755 /usr/local/sbin/nodemigrate-systemd-entrypoint

ENTRYPOINT ["/usr/local/sbin/nodemigrate-systemd-entrypoint"]
