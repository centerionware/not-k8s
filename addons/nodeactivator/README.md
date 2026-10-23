# Node Activator addon

Node Activator is an optional in-cluster HTTP addon. It is not linked into
`notk8s` and does not add a Service API or an autoscaling API. Kubernetes sees
ordinary Services, an `autoscaling/v2` HPA, an `external.metrics.k8s.io`
APIService, and a Pod that implements the extra traffic handling.

The front Service selects the Node Activator Pod. The addon buffers requests
while its backend Service has no ready endpoints, retries connection failures,
and reports its queued plus in-flight request count as the standard External
metric `activator_requests_active`. The HPA can then scale the backend
Deployment from zero. When requests finish, the metric returns to zero and the
HPA's standard downscale stabilization window determines when the Deployment
returns to zero.

## Build and install

The bootstrapper can install both optional addons after the control plane is
ready. Build or publish the activator image first so every node can pull it:

```sh
./target/debug/nodebootstrap --metrics-server \
  --nodeactivator-image=registry.example/nodeactivator:v1
```

For migration to nodestore, use the corresponding migration options:

```sh
notk8s migrate from=k3s to=nodestore metrics-server=true \
  nodeactivator-image=registry.example/nodeactivator:v1
```

Metrics Server supplies resource metrics such as CPU and memory. Kubernetes
1.37 HPA scale-to-zero uses object or external metrics; the Node Activator
exposes its request count through the standard External Metrics API. Either
addon can be enabled independently.

Build from the repository root and make the image available to the cluster's
nodes. The image contains both the activator and its PodCertificate signer:

```sh
docker build -f addons/nodeactivator/Dockerfile -t nodeactivator:latest .
```

Kubernetes 1.37's stable `podCertificate` projected volume gives each
nodeactivator Pod a kubelet-generated private key and rotates its credential
bundle. The addon includes a signer controller Pod because Kubernetes defines
the PodCertificateRequest API and projection, but does not provide a general
serving-certificate issuer. The signer only signs requests for the
nodeactivator ServiceAccount and the metrics Service. Keep its CA private key
in the `nodeactivator-signer-ca` Secret; the signer never shares that key with
the activator or other cluster components.

The bootstrap installer creates and retains a dedicated signer CA Secret,
mounts the aggregation proxy CA directly from
`extension-apiserver-authentication`, renders the APIService CA bundle, and
applies the Pod RBAC and Deployments. It does not reuse the proxy CA or any
other component certificate for serving. Both the projected `podCertificate`
volume and `certificates.k8s.io/v1` PodCertificateRequest API require
Kubernetes 1.37 or newer.

The metrics server validates the APIService aggregator's mTLS client
certificate and uses a `SubjectAccessReview` for every metric request. Its
ServiceAccount is bound to `system:auth-delegator` for that check. The signer
has a namespace Role that can read Pods and Services and update only
PodCertificateRequest status in `kube-system`.

## Connect an application

Keep a backend Service selecting the application Pods. Make a front Service
select the addon and set the backend URL annotation. The `enabled` label lets
the addon watch only Services that it owns.

```yaml
apiVersion: v1
kind: Service
metadata:
  name: app-backend
  namespace: default
spec:
  selector:
    app: app
  ports:
  - port: 80
    targetPort: http
---
apiVersion: v1
kind: Service
metadata:
  name: app
  namespace: default
  labels:
    activator.notk8s.io/enabled: "true"
  annotations:
    activator.notk8s.io/backend-url: http://app-backend.default.svc.cluster.local:80
spec:
  selector:
    app.kubernetes.io/name: nodeactivator
  ports:
  - port: 80
    targetPort: http
```

Start the application Deployment above zero so the HPA can observe a normal
scale-down and record its `ScaledToZero` condition. Kubernetes 1.37 then lets
that HPA wake it from zero using the external metric. A manual scale to zero
pauses the HPA until the Deployment is manually raised again. The HPA uses
only standard Kubernetes fields; the external metric selector chooses the
front Service by name:

```yaml
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: app
  namespace: default
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: app
  minReplicas: 0
  maxReplicas: 10
  behavior:
    scaleDown:
      stabilizationWindowSeconds: 120
  metrics:
  - type: External
    external:
      metric:
        name: activator_requests_active
        selector:
          matchLabels:
            service: app
      target:
        type: Value
        value: "1"
```

Use the front Service's DNS name in the HTTP `Host` header (for example,
`app.default.svc`). Requests are buffered in memory up to 1 MiB each; the
backend response limit is 2 MiB. The single addon replica accepts at most 16
active requests and returns 503 when that bound is reached. Requests wait up
to `ACTIVATOR_TIMEOUT_SECONDS` (180 seconds by default) for the backend to
accept a connection. This first implementation handles HTTP traffic; it does
not proxy TLS passthrough or arbitrary TCP protocols. Run one addon replica so
the external metric sees the same in-memory request count as the HTTP listener.
