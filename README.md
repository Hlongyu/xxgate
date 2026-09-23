# XXGate

Rust 编写的 ChatGPT OAuth 账户网关。对外提供 Responses HTTP/SSE、Search JSON、独立 `sk-` Key 和管理员后台；按账户追踪套餐、5h/7days/30days 额度、Token 用量与人民币费用。

## 本地运行

需要 Rust 1.96+、Python 3 和 Docker。开发脚本使用独立的 PostgreSQL 15 容器和 `xxgate` 数据卷。

```sh
python3 scripts/dev.py
```

首次启动会在 `.xxgate/local-env.json` 创建随机管理员密码与 32 字节加密主密钥，文件权限为 `0600`。后台默认地址为 `http://127.0.0.1:8787`；端口占用时可运行 `XXGATE_BIND=127.0.0.1:8797 python3 scripts/dev.py`。后续启动保留数据库、密码和主密钥。

打开后台，用该文件中的 `XXGATE_ADMIN_PASSWORD` 登录，依次完成：

1. 添加账户，默认选择链接授权：复制链接到浏览器登录，再把回调链接粘贴回后台；也可选择设备码授权或导入已有 OAuth 凭证 JSON。
2. 在模型与价格中点击「从所有账户获取模型」，再选择模型填写人民币单价和 Fast 倍率。
3. 手动启用账户。新账户默认停用；重新授权、额度恢复都不会自动启用。
4. 创建 API Key 并选择分组，确保该组包含已启用的账户。账户可加入多个分组，每个 Key 只属于一个分组；现有账户与 Key 自动归入默认分组。完整密钥仅显示一次。接入方的 Base URL 使用 `http://127.0.0.1:8787/v1`（修改端口后使用对应地址）。

上游成功返回的模型会自动加入模型目录；价格由管理员填写，不自动抓取或推测。图片生成与编辑通过 Responses 的 `image_generation` 工具表达，真实可用范围取决于账户授权和上游能力。

## 链接授权

后台「添加账户 → 链接授权」在本地生成带 `state` 与 S256 PKCE 的授权链接，有效期 15 分钟。浏览器授权后会跳转到 `http://localhost:1455/auth/callback?code=…&state=…`，复制地址栏中的完整链接到后台，点击「完成授权」。XXGate 不启动 1455 端口的回调监听；因此 localhost 页面打不开时仍可复制该地址。

生成链接不访问 OpenAI；浏览器负责登录，网关负责随后向 `/oauth/token` 兑换凭证。回调地址和 state 必须与当前流程匹配，每个流程只允许提交一次兑换。服务重启、过期或兑换失败后应重新生成链接。PKCE verifier 只在网关内存中保存，回调 URL、授权码和令牌不写入审计。

管理接口为 `POST /api/admin/oauth/browser`（生成链接）、`POST /api/admin/oauth/browser/{id}/complete`（JSON `callback_url`）、`GET /api/admin/oauth/browser/{id}`（查询结果）、`DELETE /api/admin/oauth/browser/{id}`（取消尚未提交的流程）。均需要管理员会话与写请求 CSRF 头。

## 账户金额统计

账户列表和详情显示 OAuth 凭证中的 Free、Plus、Pro 等套餐；缺少或未识别的套餐显示未知。打开页面只读取本地凭证元数据，不刷新授权、不请求上游，也不返回令牌。套餐来自授权时的声明，刷新授权后可更新。

Free 默认展示 30days，Plus/Pro 默认展示 5h 与 7days；实际采集到的主额度窗口优先于套餐声明。30days 是上游的 30 天窗口，不是自然月。未采集到的使用率、到期时间与周期金额保持未知。

账户页各额度周期的金额、Token 和额度金额估算使用增量汇总：每个请求完成时，与请求终态在同一事务内按结束时间累计。重复提交终态不会重复计数，历史请求沿用完成时已记录的价格；打开账户页不重新扫描请求 JSON 或主动向上游采集额度。

统计区间为 `(开始时间, 结束时间]`。完整 UTC 小时读取汇总，首尾不足一小时的部分读取精简金额明细，保留时间边界精度；7days/30days 额度估算排除 Spark 独立额度池和未调用上游的请求。普通金额窗口继续包含全部已结束、已归属该账户的请求，并单独统计未计价数量。

金额明细、对应小时汇总及额度样本至少保留最近 31 天，每小时清理；缩短请求详情保留期不会丢掉这段期间的统计。总览使用的长期小时用量汇总保持原有保留策略。迁移 0013 从仍存在的最近 31 天请求回填缺失明细，不覆盖已冻结的金额。早于旧 8 天保留范围且无法确认完整的周期标注“部分历史”，对应不完整样本不用于金额估算；已清理的历史明细无法补回。

## 请求模型路由

请求详情顶部显示客户端请求模型、发往上游的模型，以及上游响应提供的模型名称。当已记录的上游请求模型与返回模型名称不同时，标记「模型存在路由」；本地模型别名映射不单独触发该标记。判定依据是响应中的名称差异，也可能包含上游别名解析或版本名称展开，不代表已验证上游内部实际执行路径。

Responses 从生命周期事件的 `response.model` 采集，覆盖流式和非流式；独立 Compact 读取有效 JSON 响应的顶层 `model`。保存最近一次有效名称，后续事件缺少该字段不会抹掉此前的值，流中断时也保留已经观察到的名称。名称作为有限长度的元数据写入请求记录的 `response_model`，不采集输出正文或工具内部同名字段。详情按需返回该字段，列表保持精简，计费沿用派发时冻结的模型价格。旧记录或响应未提供有效名称时显示「未记录 / 无法判断」，不推测或回填；Search 不显示模型路由区块。

## 账户额度重置

账户卡片的「查询与重置」或账户详情的「查询与重置额度」可以查询 OAuth 账户的剩余可用重置次数，列出全部重置凭据的状态、获得时间和到期时间。过期、已使用、使用中及未知类型不参与选择；无到期时间的凭据排在有到期时间的凭据之后。查询失败时保留最近成功结果并显示错误，不将未知显示为零。

「重置一次」在执行前重新查询上游，优先使用最早到期的有效 Codex 重置凭据。若列表已变化，要求重新查询；不会直接改用另一张凭据。成功后刷新次数和 5h/7days 额度。重置不会自动启用已停用的账户，也不修改 Token 用量或人民币计价历史。

执行前将操作 ID 与选定凭据 ID 持久化并写入审计。重复请求返回同一结果；超时、响应解析失败或进程中断后，仅可核实并重试原操作，复用上游 `redeem_request_id` 和 `credit_id`，避免额外消耗。浏览器保存未确认操作的标识，重开页面后可核实已完成结果。查询和重置都需要管理员登录；写接口要求 CSRF 头。接口：

- `GET /api/admin/accounts/{id}/reset-credits`：最近成功快照、当前可用数量、下一张凭据和未确认操作；可带 `operation_id` 查询已持久化结果。
- `POST /api/admin/accounts/{id}/reset-credits/refresh`：查询最新完整列表。
- `POST /api/admin/accounts/{id}/reset-credits/consume`：JSON 为 `operation_id`（UUID）与 `expected_credit_id`（查询返回的下一张凭据 ID）。返回 `reset`、`nothing_to_reset`、`no_credit` 或 `already_redeemed`，以及刷新失败的独立错误列表。

上游采用 Codex 的 `/wham/rate-limit-reset-credits` 与 `/consume` 协议，复用账户出站认证头、客户端配置和 HTTP 传输。网关不购买或生成重置次数。

## API 示例

```sh
curl http://127.0.0.1:8787/v1/responses \
  -H "Authorization: Bearer $XXGATE_API_KEY" \
  -H 'Content-Type: application/json' \
  -H 'session-id: example-session' \
  -H 'thread-id: example-session' \
  -d '{"model":"your-configured-model","input":"你好","stream":true}'
```

支持 `POST /v1/responses`、`POST /responses`、`GET /v1/models` 和 `GET /models`。`stream=false` 返回最终 JSON；内部仍以 SSE 调用 Codex 上游。支持 `session-id`/`session_id`、`thread-id`/`thread_id` 和 Codex `client_metadata` 身份字段；没有明确 session 时依次使用 `conversation-id`/`conversation_id`、thread 身份；`prompt_cache_key` 仅表示缓存分区，不作为会话标识。缺少 thread 时使用 conversation 身份或沿用解析出的 session。全部稳定字段缺失时按独立请求放行（`stateless=true`），临时调度标识仅用于本次请求，不创建持久会话绑定，不按提示词、IP 或 Key 猜测会话；同一字段的别名发生冲突时返回 `identity_conflict`。

入站校验发现同组无可用账户时，流式与非流式请求都直接返回 HTTP 503，错误码为 `no_available_account`。同组有可用账户但并发满载时允许限时排队，流式请求可提前返回 HTTP 200；排队心跳为 `: xxgate.queue_heartbeat request_id=…` SSE 注释（无 `data:` / `event:`），避免中间层把它计为模型首字，后续失败通过 `response.failed` 表达。排队期间失去最后一个可用账户会立即结束等待。调用方应以流内终态判断成功，不仅看 HTTP 状态。所有 Responses 请求（包括校验失败）带 `x-request-id`，可在后台定位。

暂不提供 WebSocket、Chat Completions、后台生成、`previous_response_id`及账户对象引用。正文中的消息、工具调用、工具结果及响应 ID 随内容透传，避免破坏密文绑定的对象身份；仅兼容还原旧版本发给客户端的 ID 别名。会话、线程和父子关系等客户端元数据继续成组映射。客户端携带的加密 reasoning、compaction、工具参数和内部内容元数据原样透传，包括首次接入、重启续接及跨账户迁移；网关不解密、不计算或保存内容摘要，也不按本地历史记录拦截密文，有效性由上游处理。升级会清理旧版本保存的密文摘要。

`x-codex-turn-state` 仅用于观察：客户端传入值仍然丢弃，不向上游转发，也不据此改变账户、路由或恢复策略。请求详情的「Turn State 观察」记录客户端传入值及每次上游响应返回值（含 HTTP 错误和恢复补发），区分未携带、空值、多值和历史未采集。普通 Responses 不回传该响应头；Compact 维持原有回传行为。值默认折叠展示，限管理员详情读取，不进入请求列表或普通运行日志；不根据字段值判断模型质量。

每侧每次采集最多 8 个值、合计 64 KiB 原始字节；超过上限明确标记截断或省略。有效 UTF-8 保留原值，其他字节使用 Base64 表示，不解析字段内部内容。入站值随请求明细保留，上游值随该请求审计事件保留；旧数据不回填。

Responses 请求可携带 `max_output_tokens`，但 Codex OAuth 上游不支持该字段，XXGate 在转发前移除顶层字段（包括 null），避免上游 400。工具参数和输入内容中的同名字段保持不变。该兼容处理不保证客户端指定的输出 Token 上限；仍按上游实际 usage 记录用量和费用。

### 上下文压缩

支持 `POST /v1/responses/compact` 和 `POST /responses/compact`，按已绑定账户转发到 Codex 上游 `/responses/compact`。接收 model、input、instructions、tools、parallel_tool_calls、reasoning、service_tier、prompt_cache_key、text、client_metadata 和 access_programs。可省略 stream 或设置 false；stream=true 返回 400。上游请求不添加 stream/store，也不发送排队心跳；成功时完整透传 JSON，包括全部 output、密文、对象 ID 和未知字段。向调用者返回可选 x-codex-turn-state，维持原有回传行为；该响应头现同时记录在请求详情中。

普通 Responses 同时支持 Remote V2：保留顶层 input 数组中的 `compaction_trigger`，以及 Codex turn metadata 的 request_kind / compaction 字段，并透传 SSE 中的 compaction 输出。按触发项，或 `request_kind=compaction` 且 `compaction.implementation=responses_compaction_v2` 识别本次 V2 操作；只有历史 `type=compaction` 或 Local 摘要标记时不标成 V2。

点击请求详情可查看“上下文压缩”的方式、执行状态及是否已返回压缩项。压缩事实存于请求 JSON 中，不需要数据库迁移；列表继续仅返回渲染白名单，详情按需加载。Compact 的顶层 usage 复用输入、缓存、输出、推理 Token 及模型价格统计，保留数值用量扩展；返回的历史工具调用不重复计费。用量缺失时标为未知，Fast 实际档位缺失时不猜测倍率。压缩输入、输出、密文不进入持久记录。

两种方式均复用鉴权、来源限制、分组、排队、账户绑定、身份映射、单次派发和取消机制。Compact 整体 JSON 返回使用 sse_idle_timeout_ms 作为派发后的读取期限，以 sse_event_limit_bytes 限制响应大小，受全局内存预算约束。客户端负责触发压缩并使用完整 output 替换上下文；网关不自动触发压缩。本轮不接入 API 的 context_management 自动压缩配置。

## Search 接口与全局按次计费

提供 `POST /v1/alpha/search`，兼容 `/alpha/search`、`/backend-api/codex/alpha/search`，另提供 `/v1/search` 与 `/search` 别名。沿用网关 `sk-` Key 鉴权、分组范围、模型映射、账户粘连和并发队列，每个请求最多派发一次，不自动重试。请求模型必须已配置并被该分组中的账户支持。

```sh
curl http://127.0.0.1:8797/v1/alpha/search \
  -H "Authorization: Bearer $XXGATE_API_KEY" \
  -H 'Content-Type: application/json' \
  -H 'session-id: example-session' \
  -H 'thread-id: example-session' \
  -d '{"id":"search-session","model":"your-configured-model","commands":{"search_query":[{"q":"Rust releases"}],"response_length":"short"}}'
```

Search 使用 Codex 的独立 `alpha/search` JSON 协议，必需 `id` 和 `model`，可带 `commands`、`input`、`settings`、`reasoning` 等字段。未声明 session/thread 时以 `id` 作为粘连范围；和 Responses 联用时应提供相同的 session/thread 身份。请求的 Search ID、搜索命令、密文输入及未知扩展字段保留，响应正文（包括 `encrypted_output` 和结构化 `results`）原样返回；不会添加 Responses 的 `stream=true` 或 `store=false`。出站认证、客户端配置和身份映射沿用账户设置，查询参数和显式 OpenAI-Beta 头保留。Search 为非流式接口，响应大小和总等待时间分别受现有事件大小、上游空闲超时配置约束。

「模型与价格 → Search 按次计费 → 设置单价」设置全局人民币单价，适用于所有账户、Key 和模型。留空表示未设置（成功次数照常统计，费用标记未计价），0 表示免费。单价最多保留 8 位小数。一次上游 2xx 且完整有效的 Search 响应计 1 次；多个查询、打开或查找命令在同一个请求中仍计一次，不追加模型 Token 费用或 Fast 倍率。错误、超时、格式错误及未派发请求不计费。已确认的上游成功即使随后客户端断开，仍保留成功次数和对应计价。

派发时冻结单价。修改单价只影响后续派发请求，历史费用和进行中请求保持原价。Search 费用计入现有账户、Key、模型和时间范围的人民币汇总，「用量费用」另外列出 Search 次数和费用。「请求追踪」可以按 Search/Responses 筛选，详情展示成功次数、当次单价、金额、账户、上游状态和耗时。搜索词、结果及密文不持久化。

管理接口：`GET /api/admin/search-price` 获取 `{version, per_call}`，`PUT` 提交同结构更新，`per_call` 为人民币十进制字符串或 null；旧 version 更新返回冲突。请求列表支持 `kind=search|responses|compact`。本功能只对独立 Search 接口计费，不将 Responses 内嵌的 web_search 工具调用合并为独立 Search 次数。

## 运行语义

- 每个入站请求默认最多调用一次上游。唯一例外是明确的 `invalid_encrypted_content` 拒绝：首次 HTTP 400，或尚未交付模型事件且没有输出/用量事实的 SSE `response.failed` / `error`，可在原账户和绑定上恢复一次，总计最多两次。传输、授权失败和过载不会自动重放。OAuth 刷新和额度查询是独立控制请求。
- 普通加密推理错误只移除 reasoning 的 `encrypted_content` 和空 content，保留摘要、ID、非空内容和其他字段，删除仅剩 type 的空条目。明确的加密工具输出解码错误只处理 function_call_output / custom_tool_call_output 的 output 数组：将 encrypted_content 片段替换为“历史结果不可用”的文字标记，保留调用关联、明文、图片及其他内容，不假定工具未执行或伪造成功结果。压缩块和工具调用参数密文不修改；这是有损恢复，无法还原被拒绝密文的原文。
- 仅带有可恢复内容的首次尝试暂存开场控制事件（最多 16 条、64 KiB，计入内存预算），遇到输出项、工具调用、用量或其他事件即提交暂存事件并关闭恢复。暂存期间使用 SSE 注释保活，心跳不延长上游空闲期限。取消、网络中断、超时、第二次失败和无可清理内容均不恢复。Search 和独立 Compact 不适用。补发前重新检查 Key、账户与模型，始终保留原容量槽和价格。请求详情记录 HTTP / SSE 触发来源、推理清理数量、不可用工具片段数量及两次尝试。
- 会话按 Key 隔离，路由限定在 Key 分组内；会话间轮转，同一线程串行，不同线程共享单 session 并发上限。账户满载时限时等待原账户；停用后仅在同组有可用目标时，等待旧代次所有在途请求收尾再迁移未发送请求。没有可用目标立即拒绝；Key 改组后原组等待请求终止，后续请求建立新绑定代次。
- 每次迁移创建新绑定代次，A→B→A 也不会复用旧代次的会话/线程等客户端身份 ID。绑定、映射和账户状态持久化，排队正文不落盘，重启不补发请求。
- 队列、并发、内存、连接超时、SSE 空闲超时、排队心跳和保留期支持热更新。已有计时器以原开始时间重新计算。
- 统计用量事实、官方额度和本地计价分别保存。人民币金额使用十进制定点数，分项保留 8 位小数；缓存 Token 从输入中拆分，推理 Token 不重复计入输出。
- Fast/priority 采用标准价格乘以模型的 `fast_multiplier`（界面默认 2，可设大于 0 至 100），输入、缓存、输出及图片计价项使用同一倍率。采用上游确认的档位；请求 Fast 但实际档位缺失时标记未知。缺少用量、缓存细分或价格时不推定费用为零。
- 凭证使用 ChaCha20-Poly1305 加密且绑定账户 ID；管理员密码使用 Argon2，Key 和会话令牌仅保存哈希。后台使用 HttpOnly/SameSite Cookie 与 CSRF 请求头。

## 上游模型与快速填价

账户授权或导入成功后会自动尝试获取上游模型，也可从账户详情单独获取，或在「模型与价格」批量获取所有账户。使用各账户的 OAuth 请求 `/models?client_version=<动态版本>`：启动后立即检查官方 `openai/codex` 最新稳定版，之后每 6 小时检查一次；成功版本持久化到数据库且不降级。同步失败继续使用已保存版本，首次运行无缓存时回退到 `0.153.4`。模型发现的版本参数、Version 请求头与标准 Codex User-Agent 版本同步更新；推理协议基线保持独立。版本更新后点击「获取模型」重新采集目录。模型获取仅保存模型名称、显示名、上下文长度和采集时间，不保存上游模型说明或提示词。批量任务在后台运行，界面展示进度和失败原因。

每个账户保存最近成功的列表，失败时保留旧列表。调度同时检查已知上游模型列表和手动设置的账户模型范围；首次成功获取到空列表表示该账户当前不支持任何已列模型。获取模型不会改变账户启用状态。新增模型自动加入可配置目录，已有别名、价格和启停配置保留。

价格表通过所有账户模型的并集快速选择名称，只需填写标准单价与 Fast 倍率；相同提供方/接入方式/上游模型共享价格，别名可复用。保存新价格版本后，历史请求已经记录的费用不重算。旧数据中的独立 Fast 价格不会自动推测成倍率，需要保存一次倍率配置后用于后续 Fast 请求。

模型目录接口证明的是上游列出的模型，具体图片生成、工具和档位能力仍由上游验证，管理员可调整模型能力开关。

## 配置与部署

Docker 生产部署及边缘代理到源站的示例链路见 [deploy/README.md](deploy/README.md)。使用根目录 `Dockerfile` 与 `compose.production.yaml`，开发环境的 `compose.yaml` 仍仅启动本地 PostgreSQL。

| 环境变量 | 说明 |
|---|---|
| `DATABASE_URL` | PostgreSQL 连接地址，必填 |
| `XXGATE_MASTER_KEY` | Base64 编码的 32 字节主密钥，必填且需备份 |
| `XXGATE_ADMIN_PASSWORD` | 首次创建管理员使用，至少 12 字节；后续启动不覆盖密码 |
| `XXGATE_BIND` | 默认 `127.0.0.1:8787` |
| `XXGATE_SECURE_COOKIES` | 非回环监听默认开启；HTTPS 部署设为 `1` |
| `XXGATE_ALLOW_HTTP_UPSTREAM` | 仅本地模拟上游测试使用，设为 `1` 允许 HTTP |
| `RUST_LOG` | tracing 日志过滤规则 |

运行参数及其默认值在后台「运行配置」中管理。队列默认 2000 条、全局并发 200、单账户默认并发 2、单 session 默认并发 2、共享内存预算 512 MiB。请求正文为解析和出站复制预留保守的 16 倍空间，因此同时可容纳的大请求数量取决于内存预算。

「运行配置 → 并发与容量 → 单 session 并发上限」对应 `session_max_inflight`，范围 1–1000，默认 2。设为 1 恢复会话串行；上调唤醒排队请求，下调只限制新增占用，已在途请求继续收尾。每个请求同时受同线程串行、session、账户和全局并发上限约束；同一线程的等待不会挡住其他可执行线程。旧配置缺少此字段时使用默认值 2，无需数据库迁移。

该上限按「网关 Key + 客户端 session」隔离，限制同一任务及其子线程，不能限制一个人主动创建多个 session 的总并发。未声明会话的独立请求仍使用各自的临时范围。并行线程共享已持久化的账户绑定与代次，首次绑定或迁移提交完成前不会放行其他线程。


```sh
cargo build --release --locked
```

生产环境使用独立数据库、受控环境变量和 HTTPS 反向代理；SSE 路由需关闭代理缓冲并允许长连接。加密主密钥应与数据库一起备份。首版为单实例，数据库 advisory lock 阻止重复运行；不提供多实例调度。发生无法确认的持久化故障时停止派发，排查数据库后重启可恢复持久化状态。

## 观测与验证

后台包括总览、账户、请求追踪、错误调查、用量费用、模型价格、Key、配置和操作审计。用量汇总可按账户、Key、模型、实际档位和整点时间区间筛选；请求详情展示绑定代次、ID 映射、配置版本、上游状态、排队/响应头/首事件/首内容/总耗时及脱敏错误。正文、工具参数、模型输出和图片字节不保存。

请求详情中的「本次请求改写」展示字段位置、传入 ID、转发 ID 与处理方式，支持搜索及显示未改写项；只采集标识字段，不采集对应内容或密文。逐字段记录从功能启用后的新请求开始，旧请求不依据累计映射推测或回填。

结构化 JSON 日志包含请求 ID；`GET /healthz` 检查数据库及派发状态。管理员认证后的 `GET /api/admin/metrics` 输出 Prometheus 文本指标，不使用会话或请求 ID 作为标签。当前未配置 OTLP 导出。

```sh
python3 scripts/test.py
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
node --check crates/xxgate-server/web/app.js
node --test crates/xxgate-server/web/tests/*.test.cjs
```

测试脚本创建随机命名的临时数据库，执行全部单元与真实 HTTP/PostgreSQL 集成测试，完成后删除临时数据库。普通 `cargo test` 会跳过需要数据库的集成测试。

已覆盖迁移、并发等待、取消、身份换代、工具关联保留、加密上下文及其绑定 ID 透传、旧别名还原与不保存摘要、热超时、SSE 分帧、一般失败不重试及 HTTP/SSE 加密上下文受限恢复、Token/Fast/图片计价、拒绝请求追踪和重启收尾。真实 ChatGPT OAuth、图片生成编辑、Codex/sub2api 长时间联调尚需使用专门测试账户验证；当前测试不读取本机 Codex 凭证，也不会向官方发起生成调用。

详细设计见 [设计基线](docs/design.md)、[模块规划](docs/modules.md)、[实现与验证记录](docs/implementation.md)。

## 失败请求诊断

新请求保存 `ingress_diagnostics`，包含接口路径（不含查询字符串）、方法、选定的请求关联头、请求体字节数和字段类型、收到的身份候选值、来源、最终会话/线程解析来源。身份值限制为 512 字节，每个头最多收集 4 个重复值；过长、类型/字符/编码异常仅保存原因和长度。元数据只提取枚举的身份字段，不保存任意内容。

入站拒绝标明鉴权、编码、读正文、JSON、身份/协议校验或入队阶段；后续失败标明派发准备、上游连接或响应处理阶段。拒绝诊断进入请求记录、`request_rejected` 审计事件及 WARN 结构化日志；下游可以按 `x-request-id` 定位，后台请求详情的「请求接入诊断」显示收到的字段、值和解析来源。持久化失败仍输出完整的受限诊断日志，避免静默丢失原因。旧请求没有采集的数据不回填。

Authorization 仅记录是否存在；Cookie、Token、API Key、提示词、工具参数、图片和密文正文不进入诊断。若经 Nginx 转发下划线身份头，需要在相关 server 配置启用 `underscores_in_headers on`；两级远端 Nginx 已随 20260911-023649 版本更新，公网身份头转发及失败诊断已验证。

## 请求缓存命中率

「请求追踪」突出每次 Responses 请求的缓存命中率（缓存输入 Token ÷ 总输入 Token），展示进度条、与前次的百分点变化，以及带标签的输入、输出、费用和耗时。详情顶部显示计算依据与前次请求入口，可一键按当前 Key 和会话查看趋势。

前次基准从保留期内同 Key、客户端会话及线程的已派发 Responses 请求查找，不受列表筛选或分页影响；Search 和未派发请求不作为基准。下降至少 20 个百分点时用橙色提示，可仅查看本页的明显下降。账户、绑定代次或模型变化单独标记，不作为同条件的下降告警；并发重叠、前次用量未知也不进行比较。

图表按本页请求从旧到新排列，仅连接可比较的相邻调用；加权命中率、最低值和下降次数均标明为本页范围。缺失、不完整、异常或零输入用量显示未知或不适用，不当作 0% 参与统计。Search 不参与缓存统计。缓存复用并非仅由会话 ID 决定，上下文前缀变化、压缩和缓存过期都可能导致命中率降低。

## 无会话标识的 Responses 请求

XXGate 接受普通 Responses 客户端，无需来自 Codex。无 session/thread/conversation 的请求使用独立临时调度范围，可以按账户并发限制同时派发；请求记录的客户端会话、线程和绑定为空，接入诊断标记 `stateless`，不会把临时绑定、映射写入持久会话表。错误或冲突的已提供标识仍返回参数错误。

临时调度标识不发给上游。会话、线程、turn、窗口、安装标识及 Codex 元数据在调用方提供时改写转发。对可解析的会话，正文元数据也会投影到上游 session-id/thread-id/x-client-request-id；已提供的 turn、窗口、安装及父线程信息补到对应 Codex 请求头，复用正文中的同一映射。真正缺失的 turn/窗口等不随机生成。`prompt_cache_key` 未提供就省略；提供时在账户与网关 Key 范围内作稳定隔离，只有缓存键也不建立会话。已有会话的缓存键与 session/thread 相同时沿用原绑定映射。仍按上游实际返回的缓存 Token 计量，不按会话猜测命中率。

对于仅提供 `prompt_cache_key` 的独立 Responses 请求，上游 `session_id`、`conversation_id` 请求头复用该键的稳定隔离值，与 sub2api 的 OAuth 转发行为对齐。它们只用于上游缓存请求的关联：内部仍为 `stateless=true`，不创建持久绑定、不按缓存键串行调度，也不据此识别为 Codex。未提供缓存键时不补充这些头；请求正文及现有缓存键映射保持原样。此兼容修复的真实缓存改善需要使用相同请求链进行线上对照验证。

普通字符串输入转换为用户消息数组；缺省 `instructions` 补为空字符串，消息中的 `system` 角色转换为 Codex 通道支持的 `developer`。保留原内容、工具关联与密文。详情标明「独立请求」，命中率仍展示，但不与其他无标识请求建立前次比较。

[OpenAI 提示缓存文档](https://developers.openai.com/api/docs/guides/prompt-caching)说明前缀缓存可自动工作，`prompt_cache_key` 为可选项；该文档不公开订阅账户通道的内部缓存键值。

## 客户端来源与账户限制

入站先识别客户端来源，再校验和解析实际提供的会话字段，最后选择账户并构造上游请求。来源和会话是独立判断：Codex 可以没有会话标识，未知来源也可以带有效会话。当前识别 Codex CLI/TUI、VS Code 及 `Codex …` 桌面客户端的版本化 User-Agent，或含 session/thread/turn 及安装或窗口标识的完整 Codex 元数据；只提供普通 session、模型名、originator 或缓存键不构成 Codex 来源依据。识别使用入站信息，不使用网关生成的出站 UA 或标识。

识别结果存入 `client_origin`（source、rule、version、evidence），证据只列字段位置；历史记录未采集时显示「来源未记录」。请求追踪可按 Codex/未知来源筛选，详情显示识别依据。识别是请求特征匹配，不替代网关 Key 鉴权，也不声称验证了软件真实性。规则集中在 `xxgate-codex/src/client_source.rs`，后续加入其他客户端的已验证规则即可扩展，当前 opencode/pi 等未匹配来源仍为合法 unknown。

账户设置中的「仅允许 Codex」默认关闭，对应账户 JSON 字段 `codex_only`。开启后，未知来源不能使用该账户；路由只在同组且允许当前来源的账户中选择，没有符合来源条件的账户返回 403 `client_source_not_allowed`，不排队。入队、出队及发送前重新检查；限制变更后的等待请求可以迁往同组允许的账户，正在执行的请求继续收尾。模型编辑等未提交该字段的账户更新保留既有开关。新建账户可提交该字段，重新授权保留现有设置。

转发补认证与 Codex 通道所需的协议字段（如 `store=false`、`stream=true`、缺省 instructions），并将已解析的会话信息投影到上游请求头；不会因来源是 Codex 就随机生成缺失的可选标识。默认服务档位不再额外填入请求。

## 管理页面性能

`GET /api/admin/requests` 返回渲染白名单：状态、模型、来源、会话入口、Token、耗时、费用浮层所需历史单价，以及精简的前次缓存比较。列表不返回 `raw_usage`、接入诊断、配置轨迹或完整请求记录。完整请求、时间线、诊断、绑定和映射在点击详情后通过 `GET /api/admin/requests/{id}` 读取。

数据库先按筛选分页，再执行账户名、历史价格及前次请求的补充查询；前次基准用 Key、会话、线程和倒序时间/ID 的部分索引查找，保持跨分页比较和原有隔离规则。字段在 SQL 中投影，完整诊断不会先传到应用再被删除。

后台 JSON 和静态资源支持 Brotli/gzip；SSE 维持不压缩的即时发送。切页立即显示加载状态，并取消旧页面请求；详情立即打开加载抽屉，与分组名称并行读取。页面不可见时跳过轮询，数据未变化不重建 DOM，刷新期间开始输入或打开浮层/详情也不会被后台重绘打断。HTTPS HTTP/2 的部署设置见 [部署说明](deploy/README.md)。


## 错误调查

侧边栏「错误调查」集中查看请求失败、接入拒绝和进程中断，默认最近 24 小时，不包含正常客户端取消；可单独筛选 cancelled。支持最近 1 小时、24 小时、7 天或最长 31 天自定义范围，以及账户、Key、模型、接口、错误码、失败阶段、上游 HTTP、会话和文本搜索。时间范围按请求开始时间统计，并受请求明细保留期限约束。

汇总展示错误请求数、类型数、影响账户与会话、最近发生时间及发生趋势。错误类型按网关错误码、失败阶段、上游 HTTP、已识别原因和参数分组，显示次数最多的 20 类；计数覆盖全部匹配记录，不受明细分页影响。点击类型可精确筛选明细，支持区分未收到上游响应和上游返回 HTTP 状态；明细分页每页 25 条，点击请求 ID 才获取完整详情，详情可反查同类错误或跳转会话。

`GET /api/admin/request-errors` 需要管理员会话，查询参数为 from/to（RFC3339，默认过去 24 小时）、account_id、key_id、model、kind、state、code、stage、upstream_status、upstream_missing、cause、param、session_id、request_id、q、limit（1–100）、offset。q 按字面进行不区分大小写的匹配，不将 SQL 通配符当成模式。所有汇总与明细使用同一数据库快照，响应仅投影页面需要的字段，不携带原始 usage、请求诊断或时间线；账户与 Key 选择项只返回 ID 和名称。

新增请求的上游错误会提取白名单原因、错误码和参数（如不支持 max_output_tokens、加密上下文无效、上下文超限），存入可选 upstream_error。原始错误文本仍仅返回当次调用方，不写入记录；输入、输出、密文和工具参数不持久化。内容策略拒绝使用 `upstream_content_policy_violation`，网络安全风险提示记录为 `cybersecurity_risk`，后台显示具体原因。Responses 非流式调用返回 HTTP 403，流式调用在失败事件中返回同一错误码与上游说明（已发送的 HTTP 200 响应头不会改写）；这类拒绝不会停用账户。对于带有会话 ID 的请求，网关持久化「网关 Key + 会话 ID」的安全拒绝记录；该会话后续调用返回 HTTP 403 与 `session_safety_blocked`，不再转发，即使更换内容、模型、线程或上游账户也一样。排队请求发送前会复查，已经发往上游的请求无法撤回。不同 Key 或不同会话互不影响，无会话 ID 的请求只返回当次拒绝。此判断不读取或比较正文，不生成内容指纹；记录不随请求历史清理或服务重启失效。历史记录无法补回未保存的原文，继续展示当时保存的通用说明。迁移 9 新增错误请求的时间索引，旧数据无需改写。

## 许可证

本项目使用 [MIT 许可证](LICENSE)。
