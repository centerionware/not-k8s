FROM ubuntu:24.04

ENV container=docker
STOPSIGNAL SIGRTMIN+3

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        clang \
        containernetworking-plugins \
        conntrack \
        containerd \
        ca-certificates \
        curl \
        ethtool \
        gpg \
        iproute2 \
        iptables \
        iputils-ping \
        jq \
        kmod \
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
    && systemctl enable containerd \
    && mkdir -p /etc/apt/keyrings \
    && curl -fsSL https://pkgs.k8s.io/core:/stable:/v1.35/deb/Release.key \
        | gpg --dearmor --yes -o /etc/apt/keyrings/kubernetes-apt-keyring.gpg \
    && printf 'deb [signed-by=/etc/apt/keyrings/kubernetes-apt-keyring.gpg] https://pkgs.k8s.io/core:/stable:/v1.35/deb/ /\n' \
        > /etc/apt/sources.list.d/kubernetes.list \
    && apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        kubeadm kubelet kubectl \
    && apt-mark hold kubeadm kubelet kubectl \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

COPY systemd-entrypoint.sh /usr/local/sbin/nodemigrate-systemd-entrypoint
RUN chmod 0755 /usr/local/sbin/nodemigrate-systemd-entrypoint

ENTRYPOINT ["/usr/local/sbin/nodemigrate-systemd-entrypoint"]
