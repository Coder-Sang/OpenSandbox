FROM sandbox-registry.cn-zhangjiakou.cr.aliyuncs.com/opensandbox/controller:v0.2.0 AS go-builder

USER 0
ENV GOTOOLCHAIN=auto CGO_ENABLED=0 GOPROXY=https://goproxy.cn,direct GOCACHE=/tmp/go-build
WORKDIR /src

COPY kubernetes/go.mod kubernetes/go.sum ./kubernetes/
RUN cd kubernetes && go mod download
COPY kubernetes ./kubernetes
RUN cd kubernetes && go build -trimpath -buildvcs=false -ldflags '-buildid= -B none' -o /out/controller ./cmd/controller
RUN cd kubernetes && go build -trimpath -buildvcs=false -ldflags '-buildid= -B none' -o /out/task-executor ./cmd/task-executor

COPY components/internal/go.mod components/internal/go.sum ./components/internal/
COPY components/execd/go.mod components/execd/go.sum ./components/execd/
RUN cd components/internal && go mod download
RUN cd components/execd && go mod download
COPY components/internal ./components/internal
COPY components/execd ./components/execd
RUN cd components/execd && go build -trimpath -buildvcs=false -ldflags '-buildid= -B none' -o /out/execd ./main.go

COPY .e2e/attack_probe.c /tmp/attack_probe.c
RUN gcc -Os -static -s -Wall -Wextra -Werror -o /out/attack-probe /tmp/attack_probe.c

RUN mkdir -p /out/libexec && \
    gcc -Os -static -s -Wall -Wextra -Werror -o /out/libexec/opensandbox-session-gate components/execd/native/session-gate.c && \
    gcc -Os -static -s -Wall -Wextra -Werror -o /out/libexec/opensandbox-launcher components/execd/native/launcher.c && \
    gcc -Os -static -s -Wall -Wextra -Werror -o /out/libexec/opensandbox-nsenter components/execd/native/nsenter.c

FROM sandbox-registry.cn-zhangjiakou.cr.aliyuncs.com/opensandbox/controller:v0.2.0 AS controller
COPY --from=go-builder /out/controller /workspace/server
USER 65532
ENTRYPOINT ["/workspace/server"]

FROM sandbox-registry.cn-zhangjiakou.cr.aliyuncs.com/opensandbox/controller:v0.2.0 AS runtime
USER 0
RUN apt-get update && \
    apt-get install -y --no-install-recommends bubblewrap ca-certificates curl libcap2-bin procps util-linux && \
    rm -rf /var/lib/apt/lists/*
RUN mkdir -p /etc/execd /opt/opensandbox /usr/local/libexec /workspace/a /workspace/b /workspace/sdk
COPY --from=go-builder /out/task-executor /opt/opensandbox/task-executor
COPY --from=go-builder /out/execd /opt/opensandbox/execd
COPY components/execd/bootstrap.sh /opt/opensandbox/bootstrap.sh
COPY --from=go-builder /out/libexec/ /usr/local/libexec/
COPY --from=go-builder /out/libexec/ /opt/opensandbox/
COPY --from=go-builder /out/attack-probe /usr/local/bin/opensandbox-attack-probe
COPY .e2e/isolation.toml /etc/execd/isolation.toml
RUN chmod 0755 /opt/opensandbox/task-executor /opt/opensandbox/execd /opt/opensandbox/bootstrap.sh /opt/opensandbox/opensandbox-* /usr/local/libexec/opensandbox-* /usr/local/bin/opensandbox-attack-probe
ENTRYPOINT ["/opt/opensandbox/task-executor"]
