# Docker 生产部署

本目录提供通用部署模板。首次部署使用新数据库；后续更新保留账户、Key、价格、授权凭证、请求历史及数据卷，不导入开发环境数据。

## 访问链路

```text
浏览器 / API 客户端
  https://xxgate.example.com
  → HTTPS 边缘 Nginx
  → 私有隧道 → 源站 Nginx
  → 稳定 upstream → blue/green XXGate 入口

可选的 sub2api 等内部调用方
  → 稳定 Nginx 入口 → blue/green XXGate 入口

xxgate → xxgate-private 网络 → xxgate-db:5432
```

`nginx/` 中的 `xxgate.example.com`、`192.0.2.10`（边缘地址）和 `192.0.2.20`（源站地址）都是示例，部署前替换为自己的域名与私有隧道地址。源站模板仅允许边缘地址访问；不要把源站或数据库直接暴露到公网。源站 Nginx 通过 `xxgate-active-upstream.inc` 指向当前颜色，监听端口保持不变。

如果调用方仍然使用 Docker 网络里的 `http://xxgate:8787`，这个别名必须指向稳定 Nginx 入口；直接指向某个 XXGate 容器会绕过蓝绿切换。可以把 Nginx 作为共享网络中的固定入口服务，或让调用方改用源站 Nginx 的私网地址。

两级代理关闭响应/请求缓冲、代理缓存、Nginx 自身压缩和自动重试，读取/发送超时为 3600 秒。XXGate 对管理 JSON、HTML、JS、CSS 协商 Brotli/gzip，Responses/SSE 不经过应用压缩层。保留 `Accept-Encoding` 与 `Content-Encoding`。

兼容 `session_id`、`thread_id`、`conversation_id` 时，各级 Nginx 的 server 块均应设置 `underscores_in_headers on`；模板已包含。HTTPS 边缘使用 `http2 on`，需要支持此语法的 Nginx；代理回源仍使用 HTTP/1.1。

## 本地构建和验证

**禁止在生产服务器编译，包括 Docker 构建。** 在开发机完成测试并构建 Linux amd64 镜像：

```sh
python3 scripts/test.py
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
node --test crates/xxgate-server/web/tests/*.test.cjs

docker buildx build --pull --platform linux/amd64 --load \
  --build-arg SYSTEM_PACKAGES_REFRESH="$(date -u +%Y%m%dT%H%M%SZ)" \
  -t local/xxgate:<tag> .
docker run --rm -i --platform linux/amd64 --read-only --entrypoint sh \
  local/xxgate:<tag> -s < scripts/check-release-certificates.sh
docker save local/xxgate:<tag> | gzip > xxgate-image.tar.gz
shasum -a 256 xxgate-image.tar.gz > xxgate-image.tar.gz.sha256
```

将 `<tag>` 替换为本次发布的唯一标记。发布构建用 `--pull` 刷新基础镜像，并为 `SYSTEM_PACKAGES_REFRESH` 传入本次唯一值以重新安装系统软件包，避免复用过期的证书层。不要使用全局 `--no-cache`：它会清除这些 RUN 步骤的 BuildKit 缓存挂载，使 Cargo registry、git 和 target 无法复用。系统包层刷新会重跑 Cargo，但保留同一 builder 的依赖与编译产物缓存；工具链或依赖变更仍可能导致必要的重编译。不要在发布构建期间启动不同资源配置的 Apple container 构建，它可能重启共享 builder。证书包检查必须通过后才能导出镜像；检查失败时停止发布。先在开发机用隔离配置验证镜像启动、管理接口和静态资源，再传输成品镜像、校验文件及部署配置。生产主机只执行 SHA-256 校验、`docker load`、候选检查、备份和 `up --no-build`。

多阶段 Dockerfile 使用 Cargo.lock、Rust 1.96.0 和 Debian Bookworm；最终镜像只包含服务程序与运行依赖。程序以 UID/GID 10001 运行，根文件系统只读。构建参数 `RUST_BUILD_IMAGE` 和 `RUNTIME_IMAGE` 可指定对应基础镜像缓存。

### 2026-10 eMudhra 证书安全更新

采用上游修复，不在业务代码中维护证书黑名单或自定义校验规则。`Cargo.lock` 使用 `rustls-webpki 0.103.16`、`webpki-roots 1.0.11` 和 `webpki-root-certs 1.0.11`：前者拒绝含已泄露公钥的证书链，两套根证书库跟进上游信任列表更新。参见 [校验库公告](https://rustsec.org/advisories/RUSTSEC-2026-0382)、[根证书库公告](https://rustsec.org/advisories/RUSTSEC-2026-0381)、[完整证书库公告](https://rustsec.org/advisories/RUSTSEC-2026-0383)和 [1.0.11 发布说明](https://github.com/rustls/webpki-roots/releases/tag/v%2F1.0.11)。更新依赖后必须重新编译，重启旧二进制不会获得修复。

XXGate 的 native-tls 和部分客户端使用系统证书库；订阅查询的 wreq 默认使用独立内置的 `webpki-root-certs`，SQLx 使用 `webpki-roots`。系统证书包升级不能代替内置根证书库升级。Dockerfile 正常安装官方 `ca-certificates` 包，发行版专用最低版本要求由 `scripts/check-release-certificates.sh` 在发布镜像中检查：当前 Bookworm 要求 `ca-certificates >= 20250419~deb12u2`（[Debian 更新记录](https://tracker.debian.org/news/1813406/accepted-ca-certificates-20250419deb12u2-source-into-oldstable-security/)）。脚本同时检查系统 CA 文件可读且非空；它不验证二进制内置根证书版本，也不能替代真实 TLS 测试。更换基础发行版时需按该发行版公告更新检查。直接运行二进制的环境还需要更新宿主系统证书库。

本机使用 Apple `container` 时，可用相同 Dockerfile 构建 amd64 镜像并导出 OCI 归档：

```sh
container build --platform linux/amd64 --cpus 4 --memory 6G \
  --pull --progress plain \
  --build-arg SYSTEM_PACKAGES_REFRESH="$(date -u +%Y%m%dT%H%M%SZ)" \
  -t local/xxgate:<tag> .
container run --rm -i --platform linux/amd64 --read-only --entrypoint sh \
  local/xxgate:<tag> -s < scripts/check-release-certificates.sh
container image save --platform linux/amd64 \
  -o xxgate-image.tar local/xxgate:<tag>
gzip xxgate-image.tar
shasum -a 256 xxgate-image.tar.gz > xxgate-image.tar.gz.sha256
```

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
docker compose --env-file .env -f compose.production.yaml up -d --no-build --wait xxgate-db
docker compose --env-file .env -f compose.production.yaml run --rm --no-deps xxgate migrate
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

## 蓝绿发布（兼容 schema）

此路径覆盖入口和工作节点。数据库、账号归属、Key、主密钥和会话绑定保持原样；蓝绿使用不同容器地址，worker 继续使用原逻辑 `node_id`。新 worker 先持有唯一 standby 锁，校验 schema/凭证后对控制面报告 `standby`，不会处理生成请求。旧 worker 排空已经接收的响应后结束后台任务，再释放账号锁；新 worker 重新加载持久状态、接管账号并报告 ready。旧监听器变成到新 worker 的转发入口，等待中的请求只发送一次。新请求在交接期间可能排队，正常流量切换不重放生成请求。

旧版必须已经支持 `/internal/deployment` 的协议 1；脚本发现旧版没有该协议时会直接拒绝。首次安装这项能力需要单独安排引导升级，不能把不支持交接的旧二进制宣称为零停机。此处没有自动执行旧停机脚本。当前生产若仍是旧版，不要直接停止它来绕过预检。

源站 Nginx 使用稳定 `xxgate_active` upstream。把 `nginx/xxgate-origin.conf` 的 upstream 和 proxy_pass 合并到**现有域名、TLS、访问限制配置**，不要直接覆盖已经运行的站点。将默认地址文件放到 `/etc/nginx/xxgate-active-upstream.inc`；它仅在 upstream 内 include，不能作为 `conf.d/*.conf` 被重复加载：

```nginx
upstream xxgate_active {
    include /etc/nginx/xxgate-active-upstream.inc;
    keepalive 32;
}
# 在现有 location 中：proxy_pass http://xxgate_active;
```

初始文件为 `server 127.0.0.1:8797;`。先 `nginx -t`，再 reload。继续保留 `proxy_next_upstream off`、SSE 关闭缓冲、Upgrade 透传及下划线身份头设置。不要设置会在发布中强制终止长连接的 `worker_shutdown_timeout`。Nginx reload 会让旧 worker 服务现存连接直至完成，参见 [Nginx 官方说明](https://nginx.org/en/docs/control.html)。

内部调用方也必须通过稳定 Nginx。若使用 `http://xxgate:8787`，该 Docker alias 应属于稳定代理；不能同时留在某个应用容器上。候选应用只使用自己的独立 alias。

### 发布命令

先按照前文在本地构建、测试 Linux amd64 镜像，导出并传输 SHA-256 校验文件。生产校验后 `docker load`。复制当前 `cluster.env` 为权限 0600 的 `green.env`，沿用全部凭证、数据库网络及 cluster token，按 [bluegreen.env.example](bluegreen.env.example) 只覆盖镜像、端口、独立 alias 和入口 ID。旧入口通常为 8797，新入口为 8798。每次必须使用没有在运行的 Compose project/alias，worker ID 保持原值。

```sh
python3 deploy/rollout-cluster.py \
  --env-file green.env --project xxgate-green \
  --source-entry xxgate-cluster-entry-1 \
  --source-worker-a xxgate-cluster-worker-a-1 \
  --source-worker-b xxgate-cluster-worker-b-1 \
  --record release-green.json
```

脚本运行于 Linux Docker 宿主机，通过数据库私网 IP 访问控制接口。它只使用已加载镜像和 `--no-build --pull never --no-deps`，先执行只读 `preflight`，检查全部旧节点/候选配置/独立 alias，再启动候选入口和两个 standby worker，逐个交接。完成后记录 `ready_for_nginx_switch`。空账号 worker 不要求具有可派发容量，但必须正确持有节点锁并报告 serving。

将 `--probe-url` 替换为调用方实际使用且发布主机能访问的稳定 Nginx origin，然后切换入口：

```sh
python3 deploy/bluegreen-switch.py \
  --backend http://127.0.0.1:8798 \
  --expected-backend http://127.0.0.1:8797 \
  --probe-url https://xxgate.example.com \
  --upstream-file /etc/nginx/xxgate-active-upstream.inc \
  --state-file bluegreen-state.json
```

切换脚本以文件锁串行化发布，检查预期旧地址，原子替换 include，执行 `nginx -t` / reload，并通过稳定入口核对候选 `boot_id`。reload 只发送信号，因此命令返回成功不能代替此检查。失败会恢复旧文件、再次 reload 并核验旧实例；若回退无法核验，明确报告 `rollback_unverified`，保留两套进程。

### 排空、回退和边界

- 默认排空期限为 300 秒，最多 3600 秒。期限内未排空则恢复旧 worker；不会取消正在生成的响应。交接期间新请求有等待延迟，受客户端超时影响，等待入口上限 256；超过上限明确返回 `handoff_capacity`，不是无限内存队列。持续长流量下应选择能容纳最长请求的发布窗口/客户端超时。
- 原有入口 WebSocket 保持连接，各轮继续路由到同一个逻辑 worker。工作节点不作为公共 WebSocket 入口。正在浏览器或设备授权时拒绝交接，避免丢失进程内授权状态。
- 账号锁只在 HTTP/流响应及后台持久化任务完成后释放。候选必须通过身份匹配的 standby 探针；同一节点仅允许一个候选，避免并发发布抢占。候选无法激活时保留旧转发入口和日志，不自动强杀或恢复数据库。此协议解决计划发布，不保证服务器、数据库或网络故障时的在途生成无损。
- 旧入口和 worker 不会被自动停止。保留它们至 Nginx 旧连接结束、旧入口 `/internal/deployment` 的 `active` 为 0、旧 worker 为 `forwarding` 且 `active` 为 0，再退役；不要用固定 sleep 或 `docker compose down` 代替确认。发布脚本停止旧 worker 的自动重启，防止未来重启成抢锁进程。
- **入口回退**：反向执行切换脚本，交换 backend/expected-backend，仍然核验实际 `boot_id`。**worker 回退**：旧 worker 已经交出所有权，不能靠切 Nginx 恢复旧执行代码；使用旧镜像启动新的 standby，再按同一交接协议回退。旧镜像也必须支持协议 1 且 schema 兼容。每轮记录保留镜像 ID 和 boot ID。
- 本轮没有修改数据库结构。只允许相同迁移版本和校验和的镜像并行运行；不兼容版本在切流前拒绝。未来 schema 变更需要先做经过验证的 expand/contract 分阶段迁移，不能直接解除迁移互斥锁或随意放宽版本校验。`upgrade-cluster.py` 是历史停机迁移工具，不属于这条发布路径。

验证：`python3 -B -m unittest discover -s deploy/tests` 使用真实本地 Nginx 验证 SSE、WebSocket、实际切流和失败回退；`XXGATE_TEST_PG_BIN=/path/to/postgresql/bin python3 scripts/test.py worker_handoff` 使用真实进程与临时数据库验证账号锁、standby 排他性、排空取消、不重复派发及 WebSocket 后续轮次。

## 更新、备份与恢复

更新前保留当前 `.env`、镜像标记、部署配置和数据库快照。以生产备份恢复隔离数据库验证候选镜像；蓝绿切换前确认候选与旧版本的 schema 兼容，并保留旧镜像以便回退。入口切换使用上面的稳定 upstream；需要修改 schema 的版本仍需按停写迁移流程执行。

```sh
cd /opt/xxgate
install -d -m 700 backups
umask 077
docker compose --env-file .env -f compose.production.yaml exec -T xxgate-db \
  pg_dump -U xxgate -d xxgate -Fc > backups/xxgate.dump
cp .env backups/production.env
```

恢复时先停止网关，再向空数据库恢复 dump，并使用配套的 `XXGATE_MASTER_KEY`。不要使用带 `-v` 的 `docker compose down` 更新服务，它会删除数据卷。应用启动只校验数据库结构。停止所有使用此数据库的 XXGate 进程后，使用新镜像执行 `xxgate-server migrate`，再启动服务。迁移 0021 不支持仅切回旧镜像回退；多节点部署见 [集群说明](../docs/cluster.md)。

迁移 0010 从最近 8 天仍存在的请求回填账户金额明细和小时汇总。升级前停止旧版本写入，避免漏算；若回退到迁移 0010 之前的程序，随后再次升级必须在停写与备份后重建相关派生统计。回退方案应在隔离副本上验证，不通过覆盖数据库来丢弃当前业务数据。

迁移 0013 为 30days 周期回填最近 31 天仍保留的请求，只补充缺失的金额/Token 明细，并重建对应小时汇总。必须在旧程序停止写入后迁移；旧版本仍会按 8 天清理，所以回退后再次升级不能仅跳过已执行的迁移，应在备份和停写后重新回填、重建汇总并重设历史完整性边界。新版本精简统计和额度样本保留 31 天，原始请求详情继续使用管理员设置的期限。候选检查包含账户套餐、30days 周期、历史完整性提示及 `/assets/account-quotas.js`。

### Docker 内部别名也经过稳定入口

`compose.ingress.production.yaml` 和 `nginx/xxgate-ingress.conf` 提供独立于应用版本的稳定 Nginx 容器。它在 `code-internal` 独占 `xxgate:8787` 别名，宿主回环映射为 `8796`；已有宿主 Nginx 只需将本服务的回源改为 `127.0.0.1:8796`。应用 entry 的 `XXGATE_INGRESS_ALIAS` 必须是独立名称，不能继续使用 `xxgate`。

将完整配置复制为 `${XXGATE_INGRESS_CONFIG_DIR}/nginx.conf`，并创建同目录 `active-upstream.inc`，内容为 `server <当前 entry 的私网 IP>:8787;`。目录整体挂载以支持原子替换 include；单文件 bind mount 会一直引用旧 inode。固定 `XXGATE_NGINX_IMAGE` 为已校验的官方镜像 digest；本地拉取/导出并附带 SHA-256 传输后，在生产仅加载并 `up -d --no-build --pull never`。

此模式的切换参数使用候选/旧 entry 的 Docker 私网 IP、宿主上的 include 路径、`--probe-url http://127.0.0.1:8796`；Nginx 命令参数为：

```sh
--nginx-test 'docker exec xxgate-ingress-ingress-1 nginx -t -c /etc/nginx/xxgate/nginx.conf' \
--nginx-reload 'docker exec xxgate-ingress-ingress-1 nginx -s reload -c /etc/nginx/xxgate/nginx.conf'
```

核验公网地址和内部别名返回相同 boot ID。应用容器被重新创建或 IP 发生变化时必须重新检查地址并切换 include；不要在发布流程之外直接重建 active entry。稳定代理自身的升级需要单独维护，不随每次 XXGate 发布重建。
