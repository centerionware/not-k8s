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
nodes:

```sh
docker build -f addons/nodeactivator/Dockerfile -t nodeactivator:latest .
```

Create a serving certificate for
`nodeactivator-metrics.kube-system.svc`. The certificate must be trusted by the
APIService's `spec.caBundle`. The metrics listener also requires a client
certificate signed by the apiserver aggregation proxy CA. For the standard
request-header setup, obtain that CA from
`extension-apiserver-authentication`:

```sh
kubectl get configmap extension-apiserver-authentication -n kube-system \
  -o go-template='{{index .data "requestheader-client-ca-file"}}' > proxy-ca.crt
kubectl create secret generic nodeactivator-serving -n kube-system \
  --from-file=tls.crt=serving.crt --from-file=tls.key=serving.key
kubectl create secret generic nodeapiserver-proxy-client-ca -n kube-system \
  --from-file=ca.crt=proxy-ca.crt
```

Replace `REPLACE_WITH_BASE64_SERVING_CA` in
[`deploy/nodeactivator.yaml`](deploy/nodeactivator.yaml) with the base64 encoded
PEM CA, then apply it:

```sh
kubectl apply -f addons/nodeactivator/deploy/nodeactivator.yaml
kubectl wait --for=condition=Available apiservice/v1beta1.external.metrics.k8s.io
```

The metrics server validates the APIService aggregator's mTLS client
certificate and uses a `SubjectAccessReview` for every metric request. Its
ServiceAccount is bound to `system:auth-delegator` for that check.

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
