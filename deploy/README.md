# Docker 生产部署

本目录提供通用部署模板。首次部署使用新数据库；后续更新保留账户、Key、价格、授权凭证、请求历史及数据卷，不导入开发环境数据。

## 访问链路

```text
浏览器 / API 客户端
  https://xxgate.example.com
  → HTTPS 边缘 Nginx
  → 私有隧道 → 源站 Nginx
  → 127.0.0.1:8797 → xxgate:8787

可选的 sub2api 等内部调用方
  → 共享 Docker 网络 → http://xxgate:8787/v1

xxgate → xxgate-private 网络 → xxgate-db:5432
```

`nginx/` 中的 `xxgate.example.com`、`192.0.2.10`（边缘地址）和 `192.0.2.20`（源站地址）都是示例，部署前替换为自己的域名与私有隧道地址。源站模板仅允许边缘地址访问；不要把源站或数据库直接暴露到公网。单机部署可由 HTTPS Nginx 直接回源 `127.0.0.1:8797`。

两级代理关闭响应/请求缓冲、代理缓存、Nginx 自身压缩和自动重试，读取/发送超时为 3600 秒。XXGate 对管理 JSON、HTML、JS、CSS 协商 Brotli/gzip，Responses/SSE 不经过应用压缩层。保留 `Accept-Encoding` 与 `Content-Encoding`。

兼容 `session_id`、`thread_id`、`conversation_id` 时，各级 Nginx 的 server 块均应设置 `underscores_in_headers on`；模板已包含。HTTPS 边缘使用 `http2 on`，需要支持此语法的 Nginx；代理回源仍使用 HTTP/1.1。

## 本地构建和验证

**禁止在生产服务器编译，包括 Docker 构建。** 在开发机完成测试并构建 Linux amd64 镜像：

```sh
python3 scripts/test.py
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
node --test crates/xxgate-server/web/tests/*.test.cjs

docker buildx build --platform linux/amd64 --load \
  -t local/xxgate:<tag> .
docker save local/xxgate:<tag> | gzip > xxgate-image.tar.gz
shasum -a 256 xxgate-image.tar.gz > xxgate-image.tar.gz.sha256
```

将 `<tag>` 替换为本次发布的唯一标记。先在开发机用隔离配置验证镜像启动、管理接口和静态资源，再传输成品镜像、校验文件及部署配置。生产主机只执行 SHA-256 校验、`docker load`、候选检查、备份和 `up --no-build`。

多阶段 Dockerfile 使用 Cargo.lock、Rust 1.96.0 和 Debian Bookworm；最终镜像只包含服务程序与运行依赖。程序以 UID/GID 10001 运行，根文件系统只读。构建参数 `RUST_BUILD_IMAGE` 和 `RUNTIME_IMAGE` 可指定对应基础镜像缓存。

## 初始化配置

将 `compose.production.yaml` 放到服务器的部署目录，例如 `/opt/xxgate`，复制 `production.env.example` 为 `.env` 并设置权限 `0600`。为数据库密码、管理员密码和 32 字节 Base64 加密主密钥生成独立随机值，不使用示例占位符。加密主密钥需要与数据库一并备份；凭证不得进入镜像或源码归档。

Compose 默认加入名为 `code-internal` 的共享 Docker 网络，用于内部调用方及上游访问。部署前创建该网络，或设置 `XXGATE_SUB2API_NETWORK` 为已有网络名称：

```sh
docker network create code-internal
```

网络已存在时跳过创建。PostgreSQL 只连接内部数据库网络，不开放宿主端口；网关宿主端口只绑定回环地址。`XXGATE_IMAGE_TAG` 必须与已导入的镜像标记一致。

```sh
cd /opt/xxgate
sha256sum -c xxgate-image.tar.gz.sha256
gunzip -c xxgate-image.tar.gz | docker load
docker compose --env-file .env -f compose.production.yaml config --quiet
docker compose --env-file .env -f compose.production.yaml up -d --no-build --wait
```

管理员 Cookie 在生产配置中启用 Secure，应通过 HTTPS 登录。内部调用方通过网关 `sk-` Key 调用 HTTP API，不使用管理员 Cookie。

## HTTPS 和日常检查

首次签发证书可使用 `nginx/xxgate-http-bootstrap.conf`，通过 Certbot 完成域名证书签发后切换为 `xxgate-edge.conf`。证书路径与域名对应，配置自动续期及 Nginx 重载。每次调整代理配置后先运行 `nginx -t`。

```sh
cd /opt/xxgate
docker compose --env-file .env -f compose.production.yaml ps
docker compose --env-file .env -f compose.production.yaml logs --tail=100 xxgate
curl --fail http://127.0.0.1:8797/healthz
curl --fail https://xxgate.example.com/healthz
```

健康检查不能替代模型兼容性验证。上游账户支持哪些模型、独立 Compact 是否可用，取决于上游能力；应使用自己的测试账户核验。

## 更新、备份与恢复

更新前保留当前 `.env`、镜像标记、部署配置和数据库快照。以生产备份恢复隔离数据库验证候选镜像；正式切换前暂停新请求并等待在途请求结束。保留旧镜像以便回退，避免新旧程序同时写入不兼容的派生统计。

```sh
cd /opt/xxgate
install -d -m 700 backups
umask 077
docker compose --env-file .env -f compose.production.yaml exec -T xxgate-db \
  pg_dump -U xxgate -d xxgate -Fc > backups/xxgate.dump
cp .env backups/production.env
```

恢复时先停止网关，再向空数据库恢复 dump，并使用配套的 `XXGATE_MASTER_KEY`。不要使用带 `-v` 的 `docker compose down` 更新服务，它会删除数据卷。应用启动自动运行数据库迁移。

迁移 0010 从最近 8 天仍存在的请求回填账户金额明细和小时汇总。升级前停止旧版本写入，避免漏算；若回退到迁移 0010 之前的程序，随后再次升级必须在停写与备份后重建相关派生统计。回退方案应在隔离副本上验证，不通过覆盖数据库来丢弃当前业务数据。

迁移 0013 为 30days 周期回填最近 31 天仍保留的请求，只补充缺失的金额/Token 明细，并重建对应小时汇总。必须在旧程序停止写入后迁移；旧版本仍会按 8 天清理，所以回退后再次升级不能仅跳过已执行的迁移，应在备份和停写后重新回填、重建汇总并重设历史完整性边界。新版本精简统计和额度样本保留 31 天，原始请求详情继续使用管理员设置的期限。候选检查包含账户套餐、30days 周期、历史完整性提示及 `/assets/account-quotas.js`。
