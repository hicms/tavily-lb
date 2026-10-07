# tavily-lb

用 Rust 实现的 Tavily API Key 池负载均衡器，是 [TavilyProxyManager](../../go/TavilyProxyManager) 的精简重写版。

客户端只需要一个 **Master Key**，tavily-lb 会把请求转发到 `https://api.tavily.com`，并在多个 Tavily API Key 之间自动选择与故障切换。单个二进制，内置 React + [Astryx](https://github.com/facebook/astryx) 管理后台和 MCP 端点，状态保存在一个 JSON 文件里，不依赖数据库。

## 功能

- **透明代理**：除 `/api`、`/admin`、`/mcp`、`/healthz` 外的所有路径（`/search`、`/extract`、`/crawl`、`/map`、`/usage` 等）原样转发给 Tavily，方法、query、body、响应头和状态码保持不变。
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
- **额度统计**：非 GET 请求得到 200 时计 1 次；每月自动清零；也可以从 Tavily `GET /usage` 同步真实用量（手动或定时）。同步时每个 Key 之间间隔 `usage_sync.request_interval`（默认 1s）；遇到 429 会立即停止本轮，剩余 Key 记为「跳过」，等下一轮再同步。
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
