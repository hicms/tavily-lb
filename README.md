# tavily-lb

用 Rust 实现的 Tavily API Key 池负载均衡器，是 [TavilyProxyManager](../../go/TavilyProxyManager) 的精简重写版。

客户端只需要一个 **Master Key**，tavily-lb 会把请求转发到 `https://api.tavily.com`，并在多个 Tavily API Key 之间自动选择与故障切换。单个二进制，内置 React + [Astryx](https://github.com/facebook/astryx) 管理后台和 MCP 端点，状态保存在一个 JSON 文件里，不依赖数据库。

## 功能

- **透明代理**：除 `/api`、`/admin`、`/mcp`、`/exa`、`/healthz` 外的所有路径（`/search`、`/extract`、`/crawl`、`/map`、`/usage` 等）原样转发给 Tavily，方法、query、body、响应头和状态码保持不变。
- **选 Key 策略**：只使用「启用、未失效、剩余额度 > 0」的 Key；先随机打乱，再按剩余额度从高到低排序，刚触发 429 的 Key 排到最后。
- **故障切换**：

  | 上游结果 | 处理 |
  |---|---|
  | 网络错误 / 超时 | 换下一个 Key |
  | 401 | 标记 Key 失效，换下一个 |
  | 429 | 冷却 `cooldown_on_429`（默认 60s），换下一个；全部 429 时返回最后一个 429。例外：`GET /usage` 的限流由所有 Key 共享，收到 429 直接返回，不冷却、不换 Key |
  | 432 / 433 | 标记额度耗尽，换下一个 |
  | 其它状态码 | 直接返回给客户端 |

  每个请求最多尝试 `max_attempts` 个 Key（默认 5），总时长不超过 `request_deadline`（默认 180s）。都失败时返回：最后一个 429，或 `504 upstream_timeout`、`502 upstream_error`、`503 no_available_keys`。
- **额度统计**：非 GET 请求得到 200 时计 1 次；每月自动清零；也可以从 Tavily `GET /usage` 同步真实用量（手动或定时）。同一个 Key、同一个出口 IP 的 usage 请求均至少间隔 60 秒，手动同步和 REST 查询共用持久化限制，重复请求返回 429 和 `Retry-After`。批量同步串行执行，每个 Key 间隔至少 60 秒，遇到 429 停止本轮；自动同步默认关闭，开启后也不会在启动时立即执行。usage 请求不换 Key 重试。
- **Hysteria2 出口池**：每个 Key 固定绑定一个出口，首次导入和新增 Key 分配到 Key 数最少的出口。绑定随状态保存，普通 API、MCP 搜索/抓取及 usage 同步走同一出口。启用出口池后，代理故障不会回退到直连。
- **认证**：Master Key 可以放在 `Authorization: Bearer`、query `api_key` / `apiKey` 或 JSON body 的 `api_key` / `apiKey` 中。转发前会去掉 Master Key，换成池里的 Key。比较使用常量时间。
- **管理后台**（`/admin`）：总览、导入 / 编辑 / 启用 / 禁用 / 删除 Key、复制完整 Key、单个或批量同步用量、重置用量、清理失效 Key、导出。
- **MCP 端点**（`/mcp`）：无状态的 Streamable HTTP，提供 `tavily-search`、`tavily-extract`、`tavily-crawl`、`tavily-map`、`tavily-usage` 五个工具，调用同样走 Key 池和故障切换。
- 每个响应带 `x-proxy-request-id`；如果上游 body 里有 `request_id`，还会带上 `x-tavily-request-id`。

与 Go 版相比，去掉了：批量注册、请求日志与统计图表、响应缓存、多用户等。

## 快速开始

需要 Rust 1.85+（edition 2024）、Node 20+ 和 pnpm。

```powershell
cd web
pnpm install
pnpm build              # 生成 web/dist，会被编译进二进制
cd ..
cargo build --release

$env:MASTER_KEY = "a-long-random-master-key"
$env:KEYS_FILE  = "C:\path\to\keys.txt"   # 可选：启动时导入，已存在的 Key 会跳过
.\target\release\tavily-lb.exe
```

然后打开 <http://127.0.0.1:8080/admin/>，用 Master Key 登录。如果没有设置 `MASTER_KEY`，首次启动会生成一个，打印在日志里并保存到状态文件。

Docker：

```bash
docker build -t tavily-lb .
docker run -d -p 8080:8080 -e MASTER_KEY=... -v tavily-data:/app/data tavily-lb
```

## 配置

见 [`config.example.toml`](config.example.toml)。查找顺序：`-c <path>`，然后 `$CONFIG`，然后当前目录的 `config.toml`。环境变量优先于配置文件：

`LISTEN_ADDR` / `PORT`、`TAVILY_BASE_URL`、`UPSTREAM_TIMEOUT`、`REQUEST_DEADLINE`、`MAX_ATTEMPTS`、`COOLDOWN_ON_429`、`MASTER_KEY`、`DATA_FILE`、`KEYS_FILE`、`ADMIN_UI`、`MCP_ENABLED`、`MONTHLY_RESET`、`USAGE_SYNC_ENABLED`、`USAGE_SYNC_INTERVAL`、`RUST_LOG`。

Key 文件格式见 [`keys.example.txt`](keys.example.txt)：每行 `key`、`key,别名` 或 `key,别名,月额度`。

状态文件（默认 `./data/state.json`）里有所有 Key 和自动生成的 Master Key。它采用原子写入，每 `flush_interval`（10s）写一次，修改操作后和退出时也会写。请妥善保管这个文件。

## Hysteria2 出口池

需要官方 [Hysteria 2 客户端](https://v2.hysteria.network/docs/getting-started/Installation/)、Python 3、PyYAML 和 curl。Rust 服务通过每个客户端的本地 SOCKS5 端口转发 HTTPS，协议连接由官方客户端处理。

```powershell
python scripts/prepare_hysteria.py --source "C:\path\nodes.yaml" --binary "C:\path\hysteria.exe" --output data/hysteria/nodes
```

此脚本读取 Clash YAML 中的 `hysteria2` 节点，串行启动并通过 IP 查询服务检查真实出口；不读取 Tavily Key、不调用 Tavily。失败节点和重复公网 IP 不会进入生成的 `data/hysteria/nodes/egress.toml`。将其中内容追加到根目录 `config.toml`，设置 `keys_file = "./keys.txt"`（放在所有 TOML 表之前），再启动服务。已有输出不会覆盖，重新准备时使用新目录。

服务自动启动配置中的 Hysteria 客户端，正常退出时回收进程。配置文件使用 JSON，字段见[官方客户端配置](https://v2.hysteria.network/docs/advanced/Full-Client-Config/)。客户端只监听回环地址；已有进程占用端口时启动失败，不接管未知监听者。服务被强制终止时，需先清理遗留的 Hysteria 子进程再启动。

`/api/keys` 返回每个 Key 的 `egress_id`，`/api/settings` 中的 `config.egress.nodes` 可查对应公网 IP。新增 Key 均匀分配，已绑定的 Key 不因删除其它 Key 或重启而迁移；移除仍被引用的出口会阻止启动。出口 IP 是准备时实测值，节点 IP 变化后需停止服务、重新检查并更新配置。

usage 限频保存在状态文件旁的 `state.usage.json`，按 Key 摘要和 IP 分别计时，在发送请求前写入。运行时使用单调时钟防止系统校时绕过限制；重启后持久化记录至少再冷却 60 秒。迁移时连同状态文件保留此文件，不要同时运行使用同一组 Key 的其它实例。相同状态路径有进程独占锁；不同状态目录和其它工具之间不共享限频记录。MCP `tavily-usage` 只读取本地汇总，不请求上游。

真实账号验证请仅串行做少量 search/extract 请求，建议设置 `max_attempts = 1`，不要运行旧的 `web/e2e-live` 批量套件。明确 401 的 Key 会被标记失效、禁用并排除，432/433 标记额度耗尽；代理超时不代表 Key 无效。未实际使用的 Key 不会为了检查而批量访问 Tavily。

## Agent 的 web search / web fetch

请求均使用 `Authorization: Bearer <Master Key>`，不要向 agent 暴露上游 Key。

| Agent 工具 | REST 请求 | MCP 工具 |
|---|---|---|
| web search | `POST /search`，`{"query":"查询内容","max_results":3}` | `tavily-search` |
| web fetch | `POST /extract`，`{"urls":["https://example.com"],"format":"markdown"}` | `tavily-extract` |

`/extract` 返回正文提取结果（`results[].raw_content`），失败 URL 在 `failed_results`；它不是任意 HTTP 方法的原始网页代理。MCP 接入地址为 `http://127.0.0.1:8080/mcp`。

## 客户端用法

REST（与 Tavily 官方 API 相同，只是把 Key 换成 Master Key）：

```bash
curl http://127.0.0.1:8080/search \
  -H "Authorization: Bearer $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"query":"rust async runtime","max_results":3}'
```

如果使用的 Tavily SDK 支持自定义 base URL，把它指向 tavily-lb，并用 Master Key 作为 API Key 即可。

MCP（Claude Code、Cursor 等支持 Streamable HTTP 的客户端）：

```json
{
  "mcpServers": {
    "tavily": {
      "type": "http",
      "url": "http://127.0.0.1:8080/mcp",
      "headers": { "Authorization": "Bearer <MASTER_KEY>" }
    }
  }
}
```

## Exa 兼容接口

`POST /exa/search` 和 `POST /exa/contents` 接受 Exa 格式的 JSON，请求实际由 Tavily 执行，结果组装为 Exa 的返回结构。支持自定义 Base URL 的客户端可使用 `http://127.0.0.1:8080/exa`，API Key 填本服务的 Master Key。认证支持 `x-api-key` 和 `Authorization: Bearer`；同时提供时必须都正确。仅接收未压缩的 `application/json`，参数放在 body 中。

```bash
curl http://127.0.0.1:8080/exa/search \
  -H "x-api-key: $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"query":"rust async runtime","numResults":3,"contents":{"highlights":true}}'

curl http://127.0.0.1:8080/exa/contents \
  -H "x-api-key: $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"urls":["https://example.com"],"text":{"maxCharacters":4000}}'
```

支持的参数子集：

| 接口 | 参数 | 行为与限制 |
|---|---|---|
| Search | `query`、`numResults` | 默认 10 条，支持 1–20 条；超过上游上限明确返回 400 |
| Search | `type` | `auto`（默认）、`fast`、`instant` 分别使用 Tavily `basic`、`fast`、`ultra-fast`，搜索排序与 Exa 不等价 |
| Search | `includeDomains`、`excludeDomains` | 最多 300 / 150 个普通主机名；不支持路径或通配符 |
| Search | `category`、`moderation` | 仅支持 `category: "news"`；`moderation` 转为 `safe_search` |
| Search | `contents.text`、`contents.highlights` | 未请求 contents 时只返回元数据；内容选项见下文 |
| Contents | `urls` 或 `ids` | 两者选一，1–100 个 HTTP(S) URL；`ids` 可使用本服务返回的 URL 型 ID，不支持 Exa 私有的非 URL ID |
| Contents | `text`、`highlights` | 均未指定（或为 null）时默认正文；显式 false 不触发默认正文 |

`text` 支持 boolean 或 `{ "maxCharacters": 4000, "includeHtmlTags": false }`。`highlights` 支持 boolean 或 `{ "query": "关注的问题", "maxCharacters": 2000 }`。字符上限为 1–1000000，按 Unicode 字符截断。Search 的内容参数位于 `contents` 内；Contents 的内容参数位于顶层。

正文来自 `raw_content`，不会用搜索摘要代替缺失正文。Search 的普通 highlights 来自 Tavily 搜索片段；显式 `highlights.query` 通过额外的 extract 请求选取相关片段。Contents highlights 使用 extract 的 query 排序片段；未给 query 时使用 “Key information and main points”。同时请求 text 和 highlights 会分别提取全文和片段，产生额外上游请求。片段选择与 Exa 的模型不等价，不返回伪造的 `highlightScores` 或 `costDollars`。

Contents 每批最多 20 个 URL，串行执行，整个请求（含重试和多次提取）共用 `request_deadline`。返回 `results` 和逐输入 URL 的 `statuses`；单页失败为 `status: "error"`、`error.tag: "CRAWL_ERROR"`，全部页面失败也可返回 HTTP 200。没有标题时用 URL 作为 `title`；未知的作者、抓取来源或费用不编造。上游整次请求失败或总超时返回错误，不会把已完成的部分批次作为完整成功返回。

成功和错误均有 `requestId`、`x-request-id` 和 `x-proxy-request-id`；错误体为 `{ "requestId": "...", "error": "...", "tag": "..." }`。Exa 请求 ID 标识整个适配请求，底层每次转发保留自己的日志 ID。上游 429 保留 `Retry-After`（如果存在）；上游请求失败的错误体不会原样回传。未知参数、`deep`/`neural` 等其它搜索模式、日期过滤、`summary`、`outputSchema`、`livecrawl`、`maxAgeHours`、子页面、HTML/页面区块选项等均返回 400，客户端不应把本接口视为 Exa 的完整功能实现。

两个接口复用原 `proxy::forward`，保持现有 Key 选择、故障切换、冷却、用量计数、Hysteria2 出口绑定及不回退直连的规则；一次 Exa 请求产生多次成功上游调用时按原规则逐次计数。原 Tavily REST、MCP、管理接口不变。不会持久化请求或响应正文。

协议依据：[Exa 官方 OpenAPI](https://exa.ai/docs/exa-spec.json)。`tests/fixtures/exa-contract.json` 保存了 2026-10-16 官方定义中相关 schema 的完整依赖闭包，用于真实本地 HTTP 响应校验（仅访问 mock 上游）：

```powershell
cargo build --locked
python -m pip install jsonschema  # 测试依赖；已安装时跳过
python tests/test_exa_contract.py
```

已用官方 `exa-py 2.25.0` 验证 search、get_contents 及返回解析。示例：

```python
import os
from exa_py import Exa

exa = Exa(api_key=os.environ["MASTER_KEY"], base_url="http://127.0.0.1:8080/exa")
hits = exa.search("rust async runtime", num_results=3, contents={"highlights": True})
pages = exa.get_contents(hits.results, text={"maxCharacters": 4000})
```

安装 `exa-py` 后，可运行 `python tests/test_exa_contract.py --sdk` 追加官方 SDK 联调测试。SDK 自带的默认内容选项可能与原始 REST 请求不同；适配层按 SDK 实际发出的请求执行。

## 管理 API

所有接口都需要 `Authorization: Bearer <MASTER_KEY>`。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/overview` | Key 数量（按状态）与额度汇总 |
| GET | `/api/settings` | 生效配置（不含 Master Key）、Master Key 来源、版本 |
| GET | `/api/keys` | Key 列表（Key 已脱敏） |
| POST | `/api/keys` | 导入：`{"text": "每行一个", "total_quota": 1000}`，返回 `{added, duplicates, errors}` |
| GET | `/api/keys/export` | 导出非失效 Key（纯文本） |
| DELETE | `/api/keys/invalid` | 删除所有失效 Key |
| PATCH | `/api/keys/{id}` | 修改 `alias` / `total_quota` / `used_quota` / `active` |
| DELETE | `/api/keys/{id}` | 删除 |
| GET | `/api/keys/{id}/raw` | 完整 Key |
| POST | `/api/keys/{id}/reset` | 已用额度清零，并清除冷却状态 |
| POST | `/api/keys/{id}/sync` | 从 Tavily 同步这个 Key 的用量 |
| GET / POST | `/api/sync` | 查看 / 启动全量同步（运行中再次启动返回 409） |

`GET /healthz` 不需要认证。

## 开发与测试

```powershell
cargo test                       # 单元测试 + 集成测试（mock 上游）
cargo clippy --all-targets

cd web
pnpm dev                         # Vite 开发服务器，/api 代理到 127.0.0.1:8080
pnpm typecheck
pnpm exec playwright install chromium   # 首次运行需要
pnpm e2e:full                    # 构建前端和 release 二进制，然后跑 Playwright e2e
pnpm e2e                         # 已构建时只跑 e2e
```

e2e 套件（`web/e2e`）会启动一个 mock Tavily 上游（`mock-upstream.mjs`）和使用全新状态文件的 release 二进制，覆盖以下内容：

- 登录、退出、过期 token 等认证流程；
- 管理后台的所有操作；
- 代理故障切换在界面上的表现（失效、冷却、耗尽）；
- 导出；
- REST 代理；
- 用官方 `@modelcontextprotocol/sdk` 客户端调用 MCP 的全部工具。

另外会检查两点：页面上每个表单控件都是 Astryx 组件；`src/` 中不出现原生 `<button>`、`<input>`、`<select>`、`<textarea>` 等标签。
