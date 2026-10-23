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

Create a dedicated serving CA and Secret (protect and rotate the CA key
according to your cluster's PKI policy):

```sh
openssl req -x509 -newkey rsa:3072 -nodes -days 365 \
  -keyout nodeactivator-ca.key -out nodeactivator-ca.crt \
  -subj /CN=nodeactivator-serving-ca \
  -addext 'basicConstraints=critical,CA:TRUE' \
  -addext 'keyUsage=critical,keyCertSign,cRLSign'
kubectl create secret generic nodeactivator-signer-ca -n kube-system \
  --from-file=ca.crt=nodeactivator-ca.crt --from-file=ca.key=nodeactivator-ca.key
```

The metrics listener also requires a client certificate signed by the
apiserver aggregation proxy CA. For the standard request-header setup, obtain
that CA from
`extension-apiserver-authentication`:

```sh
kubectl get configmap extension-apiserver-authentication -n kube-system \
  -o go-template='{{index .data "requestheader-client-ca-file"}}' > proxy-ca.crt
kubectl create secret generic nodeapiserver-proxy-client-ca -n kube-system \
  --from-file=ca.crt=proxy-ca.crt
```

Replace `REPLACE_WITH_BASE64_SERVING_CA` in
[`deploy/nodeactivator.yaml`](deploy/nodeactivator.yaml) with the base64 encoded
PEM contents of `nodeactivator-ca.crt`, then apply it. Both the projected
`podCertificate` volume and `certificates.k8s.io/v1` PodCertificateRequest API
require Kubernetes 1.37 or newer.

```sh
kubectl apply -f addons/nodeactivator/deploy/nodeactivator.yaml
kubectl wait --for=condition=Available apiservice/v1beta1.external.metrics.k8s.io
```

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

The application Deployment can start with zero replicas. The HPA uses only
standard Kubernetes fields; the external metric selector chooses the front
Service by name:

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
