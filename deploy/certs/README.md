# 证书目录

把本目录挂载到容器/服务端的 `/etc/rdep/certs`（或直接放这里）。需要：

- **service**：`server.crt` / `server.key`（该 service 的 TLS 身份）
- **forwarder**：`server.crt` / `server.key`（forwarder 的 TLS 身份；同时它也是各 service
  信任的「forwarder CA」）
- **client**：信任对端证书（GUI 连接时把 `.crt` 作为 CA 证书导入）

> 本目录默认只保留本说明，真实私钥**不要提交到仓库**。

## 自签证书（开发/内网）

```bash
# 生成一个带 SAN 的自签证书（CA:FALSE + serverAuth，rustls 0.23 要求）
openssl req -x509 -newkey rsa:4096 -nodes -days 3650 \
  -keyout server.key -out server.crt \
  -subj "/CN=forwarder.example.com" \
  -addext "subjectAltName=DNS:forwarder.example.com" \
  -addext "basicConstraints=CA:FALSE" \
  -addext "extendedKeyUsage=serverAuth"
```

- forwarder 与 service 可各自生成一份；service 侧把 **forwarder 的 `server.crt`** 作为
  `RDEP_FWD_CA` 信任。
- client 连接时把对端的 `.crt` 当作 CA 证书填入「CA证书」。

## 正式证书（公网）

用 certbot / 内部 CA 签发，把 `fullchain.pem`→`server.crt`、`privkey.pem`→`server.key`，
并确保证书含 `serverAuth` EKU 与正确的 SAN。续期后 reload 进程即可。
