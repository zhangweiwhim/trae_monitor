# Trae 企业版用量监控 → 飞书图表卡片

用 Rust 实现：调用 [Trae 企业版 OpenAPI](https://docs.trae.cn/enterprise/trae-cn-enterprise-api)
拉取成员 **AI 使用详情**，组装成一张**可交互的图表卡片**，推送到**飞书群或个人**。
所有接口凭证、查询范围与飞书目标都集中在 `config.yaml` 中维护。

## 功能

- **鉴权**：`POST /openapi/v1/auth/token`（`app_id` / `app_secret` 换 `access_token`）
- **拉取用户使用详情**：
  - `GET  /openapi/v1/users` 成员列表（始终自动全量拉取，再按配置黑名单排除）
  - `POST /openapi/v1/statistics/user-metrics` 成员 AI 用量 + 每日趋势
  - `POST /openapi/v1/statistics/user-model-usage` 成员模型 token 消耗
  - `GET  /openapi/v1/statistics/active-users` 活跃成员数
- **组装卡片**：飞书交互式卡片（JSON 2.0），每个图表/表格前都附一段**说明文案**：
  1. 成员代码贡献（AI vs 非 AI 提交行数，堆叠柱状图，可悬停/按图例筛选）
  2. 成员 AI 代码生成率（柱状图）
  3. 每日 AI 生成代码行数趋势（折线图）
  4. 成员模型 Token 消耗（**表格**：成员 / 模型 / 用途 / 来源 / 输入Token / 输出Token）
- **推送飞书**：
  - 自建应用 API（推荐）：`im/v1/messages`，`targets` 列表可**同时**发到多个**群**(`chat_id`)与**个人**(`open_id`/`user_id`/`union_id`/`email`)
  - 群自定义机器人 Webhook：可选 HMAC-SHA256 签名

## 配置

复制 `config.yaml` 并填入真实凭证（建议另存为 `config.local.yaml`，已在 `.gitignore` 中）：

```yaml
trae:
  base_url: "https://console.enterprise.trae.cn"   # 或企业自定义域名
  app_id: "YOUR_TRAE_APP_ID"
  app_secret: "YOUR_TRAE_APP_SECRET"
  query:
    days: 7                 # 统计天数(1~180)；start_date = end_date-(days-1) 自动回溯
    end_date: ""            # 留空取今天；start_date 始终由 days 自动换算
    timezone: "Asia/Shanghai"
    # 成员始终自动全量拉取；下面是黑名单（命中 user_id 或 email 即排除），留空 => 全部成员
    exclude_user_ids: []
    exclude_emails: []

feishu:
  app_id: "cli_xxx"         # 自建应用
  app_secret: "xxx"
  # targets 可同时配置多个，群与个人混合并存，逐个发送
  targets:
    - type: "chat_id"       # chat_id(群) | open_id/user_id/union_id/email(个人)
      id: "oc_xxx"
    - type: "open_id"
      id: "ou_xxx"
  webhook_url: ""           # 若填写则改走群机器人 Webhook（忽略 targets）
  webhook_secret: ""
  card_title: "Trae AI 使用详情周报"
```

### 推送到「群」和/或「个人」

- 在 `targets` 列表里按需添加多条，每条是 `{type, id}`，群与个人可混合并存：
  - 群：`type: chat_id`，`id` 填群的 `oc_xxx`
  - 个人：`type: open_id`（或 `email`/`user_id`/`union_id`），`id` 填对应值
- 单个目标失败不影响其它目标，只要有一个成功即视为推送成功。
- 仅 Webhook：填 `webhook_url`（只能发到机器人所在群，会忽略 `targets`）。

## 运行

```bash
# 离线预览卡片 JSON（不读配置、不联网，用内置样例数据）
cargo run --release -- --sample

# 拉取数据但只打印卡片、不推送
cargo run --release -- --config ./config.yaml --dry-run

# 正式拉取并推送到飞书
cargo run --release -- --config ./config.yaml
```

参数：`--config <path>`（默认 `config.yaml`）、`--dry-run`、`--sample`。
日志级别用环境变量控制：`RUST_LOG=debug cargo run ...`。

## 定时执行

可用系统 cron（每天 9:00 推送昨日及之前 7 天）：

```cron
0 9 * * *  cd /path/to/new_monitor && ./target/release/trae-usage-monitor --config ./config.local.yaml >> monitor.log 2>&1
```

## 说明 / 注意

- Trae OpenAPI 限速：读 2 req/s、写 1 req/s，超限返回 HTTP 429（`code: 64290`）。
  本工具调用次数很少，正常不会触发。
- `user-metrics` / `user-model-usage` 单次最多 100 个成员，已在代码中截断。
- 字段名以官方文档为准；接口若调整字段，调整 `src/trae.rs` 中的数据模型即可。
