//! Trae 企业版 OpenAPI 客户端。
//!
//! 覆盖：鉴权、成员列表、成员 AI 用量、成员模型用量、活跃成员数。
//! 文档：https://docs.trae.cn/enterprise/trae-cn-enterprise-api

// 数据模型完整映射了接口响应字段，部分字段保留作为文档/后续使用，故放开 dead_code。
#![allow(dead_code)]

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{DateRange, TraeConfig};

/// 请求间最小间隔：Trae 读限速 2 req/s、写 1 req/s，留足余量。
const REQUEST_SPACING: Duration = Duration::from_millis(550);
/// 单个请求遇到 429 / token 过期时的最大重试次数。
const MAX_RETRY: u32 = 4;
/// access_token 过期的业务错误码。
const TOKEN_EXPIRED_CODE: i64 = 1002;
/// user-metrics / user-model-usage 单次成员数上限。
pub const MAX_BATCH_USERS: usize = 100;

/// 统一响应包装：`{ code, message, request_id, data }`，成功时 code == 0。
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    // 个别网关可能在成功响应里省略 code，缺省按成功(0)处理，避免误判为错误。
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    data: Option<T>,
}

impl<T> Envelope<T> {
    fn into_data(self, ctx: &str) -> Result<T> {
        if self.code != 0 {
            return Err(anyhow!("Trae 接口 {ctx} 返回错误 code={} message={}", self.code, self.message));
        }
        self.data
            .ok_or_else(|| anyhow!("Trae 接口 {ctx} 返回 code=0 但 data 为空"))
    }
}

// ----------------------------- 数据模型 ------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    #[serde(default)]
    pub user_id: i64,
    #[serde(default)]
    pub user_name: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub join_time: i64,
}

#[derive(Debug, Deserialize)]
struct Pagination {
    #[serde(default)]
    total_pages: i64,
}

#[derive(Debug, Deserialize)]
struct UserListData {
    #[serde(default)]
    items: Vec<User>,
    #[serde(default)]
    pagination: Option<Pagination>,
}

/// 成员 AI 用量核心指标（user-metrics 与 daily_trend 共用同一组字段）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Metrics {
    #[serde(default)]
    pub ai_generation_rate: f64,
    #[serde(default)]
    pub ai_contribution_rate: f64,
    #[serde(default)]
    pub ai_generated_code_lines: i64,
    #[serde(default)]
    pub chat_code_gen_lines: i64,
    #[serde(default)]
    pub chat_code_accept_lines: i64,
    #[serde(default)]
    pub cue_recommend_count: i64,
    #[serde(default)]
    pub cue_recommend_accepted: i64,
    #[serde(default)]
    pub ai_commit_lines: i64,
    #[serde(default)]
    pub non_ai_commit_lines: i64,
    #[serde(default)]
    pub total_commit_lines: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DailyMetrics {
    #[serde(default)]
    pub date: String,
    #[serde(flatten)]
    pub metrics: Metrics,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserMetrics {
    #[serde(default)]
    pub user_id: i64,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub metrics: Metrics,
    #[serde(default)]
    pub daily_trend: Vec<DailyMetrics>,
}

#[derive(Debug, Deserialize)]
struct UserMetricsData {
    #[serde(default)]
    users: Vec<UserMetrics>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelUsage {
    #[serde(default)]
    pub model_name: String,
    #[serde(default)]
    pub model_type: String,
    #[serde(default)]
    pub model_source: String,
    #[serde(default)]
    pub usage: TokenUsage,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserModelUsage {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub model_usage: Vec<ModelUsage>,
}

#[derive(Debug, Deserialize)]
struct ModelUsageData {
    #[serde(default)]
    items: Vec<UserModelUsage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ActiveUsers {
    #[serde(default)]
    pub total_active_users: i64,
    #[serde(default)]
    pub ide_active_users: i64,
    #[serde(default)]
    pub plugin_active_users: i64,
    #[serde(default)]
    pub cli_active_users: i64,
}

/// 拉取到的全部统计数据，交给 chart 模块组装卡片。
#[derive(Debug, Default)]
pub struct UsageReport {
    pub date_range: Option<DateRange>,
    pub active_users: ActiveUsers,
    pub user_metrics: Vec<UserMetrics>,
    pub model_usage: Vec<UserModelUsage>,
}

// ----------------------------- 客户端 --------------------------------------

pub struct TraeClient {
    http: reqwest::Client,
    cfg: TraeConfig,
    // 内部可变：鉴权 / token 过期重试时需在 &self 方法里刷新。
    token: Mutex<Option<String>>,
}

impl TraeClient {
    pub fn new(cfg: TraeConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("构建 HTTP 客户端失败")?;
        Ok(Self { http, cfg, token: Mutex::new(None) })
    }

    /// 拼接请求 URL。会容忍 base_url 误带 `/openapi` 或 `/openapi/v1` 后缀，避免重复前缀导致 404。
    fn url(&self, path: &str) -> String {
        let mut base = self.cfg.base_url.trim_end_matches('/');
        for suffix in ["/openapi/v1", "/openapi"] {
            if let Some(stripped) = base.strip_suffix(suffix) {
                base = stripped.trim_end_matches('/');
                break;
            }
        }
        format!("{base}/openapi/v1/{}", path.trim_start_matches('/'))
    }

    fn token(&self) -> Result<String> {
        self.token
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("尚未鉴权，请先调用 authenticate()"))
    }

    /// 请求间隔限速。调用顺序固定，固定 sleep 即可稳定低于读/写限速。
    async fn throttle(&self) {
        tokio::time::sleep(REQUEST_SPACING).await;
    }

    /// 解析 429 响应的 Retry-After（秒），缺省 1s，封顶 30s。
    fn retry_after_secs(resp: &reqwest::Response) -> u64 {
        resp.headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(1)
            .clamp(1, 30)
    }

    /// POST /openapi/v1/auth/token —— 用 app_id/app_secret 换取 access_token。
    pub async fn authenticate(&self) -> Result<()> {
        let url = self.url("auth/token");
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.throttle().await;
            let resp = self
                .http
                .post(&url)
                .json(&json!({ "app_id": self.cfg.app_id, "app_secret": self.cfg.app_secret }))
                .send()
                .await
                .context("请求鉴权接口失败")?;

            let status = resp.status();
            if status.as_u16() == 429 && attempt <= MAX_RETRY {
                let wait = Self::retry_after_secs(&resp);
                tracing::warn!("鉴权触发限流(429)，{wait}s 后重试 ({attempt}/{MAX_RETRY})");
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }

            let body: Value = resp.json().await.context("鉴权响应不是合法 JSON")?;
            if !status.is_success() {
                return Err(anyhow!("鉴权接口 HTTP {status}: {body}"));
            }

            // access_token 可能在顶层，也可能被包在统一 data 信封里，两种都兼容。
            let token = body
                .get("access_token")
                .or_else(|| body.get("data").and_then(|d| d.get("access_token")))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("鉴权响应中找不到 access_token: {body}"))?;

            *self.token.lock().unwrap() = Some(token.to_string());
            tracing::info!("Trae 鉴权成功");
            return Ok(());
        }
    }

    /// 统一的带鉴权请求：限速 + 429 重试 + token 过期自动重新鉴权。
    async fn call<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
        ctx: &str,
    ) -> Result<T> {
        let url = self.url(path);
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.throttle().await;
            let token = self.token()?;
            let mut req = self.http.request(method.clone(), &url).bearer_auth(token);
            if !query.is_empty() {
                req = req.query(query);
            }
            if let Some(ref b) = body {
                req = req.json(b);
            }
            let resp = req.send().await.with_context(|| format!("请求 {ctx} 失败"))?;

            let status = resp.status();
            if status.as_u16() == 429 && attempt <= MAX_RETRY {
                let wait = Self::retry_after_secs(&resp);
                tracing::warn!("{ctx} 触发限流(429)，{wait}s 后重试 ({attempt}/{MAX_RETRY})");
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }

            let env: Envelope<T> =
                resp.json().await.with_context(|| format!("{ctx} 响应解析失败"))?;

            // token 过期：重新鉴权后重试一次。
            if env.code == TOKEN_EXPIRED_CODE && attempt <= MAX_RETRY {
                tracing::warn!("{ctx} access_token 过期，重新鉴权后重试");
                self.authenticate().await?;
                continue;
            }
            return env.into_data(ctx);
        }
    }

    /// GET /openapi/v1/users —— 分页拉取成员，最多返回 `limit` 个。
    pub async fn list_users(&self, limit: usize) -> Result<Vec<User>> {
        let mut out = Vec::new();
        let mut page: i64 = 1;
        let page_size: i64 = 100;
        loop {
            if out.len() >= limit {
                break;
            }
            let data: UserListData = self
                .call(
                    reqwest::Method::GET,
                    "users",
                    &[("page", page.to_string()), ("page_size", page_size.to_string())],
                    None,
                    "成员列表",
                )
                .await?;

            let total_pages = data.pagination.as_ref().map(|p| p.total_pages);
            let got = data.items.len() as i64;
            out.extend(data.items);

            // 终止条件：达到上限 / 本页不满（最后一页）/ 有分页信息且已到末页。
            // 不再因 pagination 缺省 total_pages=1 而过早停止。
            let last_page = got < page_size;
            let pages_done = total_pages.map_or(false, |tp| page >= tp.max(1));
            if last_page || pages_done {
                break;
            }
            page += 1;
        }
        out.truncate(limit);
        Ok(out)
    }

    /// POST /openapi/v1/statistics/user-metrics —— 成员 AI 用量及每日趋势。
    pub async fn user_metrics(
        &self,
        range: &DateRange,
        user_ids: &[i64],
        emails: &[String],
    ) -> Result<Vec<UserMetrics>> {
        let mut body = json!({
            "start_date": range.start_date,
            "end_date": range.end_date,
            "timezone": range.timezone,
        });
        // user_ids 优先；否则用 emails。
        if !user_ids.is_empty() {
            body["user_ids"] = json!(user_ids);
        } else if !emails.is_empty() {
            body["emails"] = json!(emails);
        } else {
            return Err(anyhow!("user-metrics 需要 user_ids 或 emails 至少其一"));
        }

        let data: UserMetricsData = self
            .call(reqwest::Method::POST, "statistics/user-metrics", &[], Some(body), "成员 AI 用量")
            .await?;
        Ok(data.users)
    }

    /// POST /openapi/v1/statistics/user-model-usage —— 成员模型 token 消耗。
    pub async fn user_model_usage(
        &self,
        range: &DateRange,
        user_ids: &[i64],
        emails: &[String],
    ) -> Result<Vec<UserModelUsage>> {
        let mut body = json!({
            "start_time": range.start_ts,
            "end_time": range.end_ts,
        });
        if !user_ids.is_empty() {
            body["user_ids"] = json!(user_ids);
        } else if !emails.is_empty() {
            body["emails"] = json!(emails);
        } else {
            return Err(anyhow!("user-model-usage 需要 user_ids 或 emails 至少其一"));
        }

        let data: ModelUsageData = self
            .call(reqwest::Method::POST, "statistics/user-model-usage", &[], Some(body), "成员模型用量")
            .await?;
        Ok(data.items)
    }

    /// GET /openapi/v1/statistics/active-users —— 活跃成员数。
    pub async fn active_users(&self, range: &DateRange) -> Result<ActiveUsers> {
        self.call(
            reqwest::Method::GET,
            "statistics/active-users",
            &[
                ("start_date", range.start_date.clone()),
                ("end_date", range.end_date.clone()),
                ("timezone", range.timezone.clone()),
            ],
            None,
            "活跃成员数",
        )
        .await
    }
}
