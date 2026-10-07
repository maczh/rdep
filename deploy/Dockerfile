# syntax=docker/dockerfile:1
# rdep-service / rdep-forwarder 通用镜像
# 构建上下文请设为仓库根目录：docker build -f deploy/Dockerfile -t rdep:service .

FROM rust:1-slim-bookworm AS builder
WORKDIR /src
# 先只拷 manifests 预取依赖，利用 layer cache
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/rdep-protocol/Cargo.toml crates/rdep-protocol/
COPY crates/rdep-service/Cargo.toml     crates/rdep-service/
COPY crates/rdep-forwarder/Cargo.toml  crates/rdep-forwarder/
COPY crates/rdep-client/Cargo.toml      crates/rdep-client/
RUN mkdir -p crates/rdep-protocol/src crates/rdep-service/src \
             crates/rdep-forwarder/src crates/rdep-client/src \
    && echo "" > crates/rdep-protocol/src/lib.rs \
    && echo "fn main(){}" > crates/rdep-service/src/main.rs \
    && echo "fn main(){}" > crates/rdep-forwarder/src/main.rs \
    && echo "fn main(){}" > crates/rdep-client/src/main.rs \
    && echo "" > crates/rdep-client/src/lib.rs \
    && cargo fetch || true
# 拷真实源码并构建（默认构建 headless 二进制，跳过 GUI 以减小体积）
COPY . .
ARG BUILD_TARGET=service   # service | forwarder
RUN if [ "$BUILD_TARGET" = "forwarder" ]; then \
        cargo build --release -p rdep-forwarder ; \
    else \
        cargo build --release -p rdep-service ; \
    fi

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 -d /var/lib/rdep -s /sbin/nologin rdep \
    && mkdir -p /var/lib/rdep/root /var/lib/rdep/scripts /etc/rdep/certs
COPY --from=builder /src/target/release/rdep-service   /usr/local/bin/rdep-service
COPY --from=builder /src/target/release/rdep-forwarder /usr/local/bin/rdep-forwarder
# 放置占位，真实证书请用 volume 挂载覆盖 /etc/rdep/certs
COPY deploy/certs/README.md /etc/rdep/certs/README.md
USER rdep
WORKDIR /var/lib/rdep
EXPOSE 8443 9443 9444 8080 8081
ENTRYPOINT ["/usr/local/bin/rdep-service"]
