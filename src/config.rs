//! 配置文件解析与查询窗口推导。

use anyhow::{anyhow, Context, Result};
use chrono::{Duration, NaiveDate, NaiveTime, TimeZone};
use chrono_tz::Tz;
use serde::Deserialize;

/// 统计窗口上限（天）。user-model-usage 接口限制 start_time/end_time 最长跨度 180 天。
const MAX_RANGE_DAYS: i64 = 180;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub trae: TraeConfig,
    pub feishu: FeishuConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TraeConfig {
    pub base_url: String,
    pub app_id: String,
    pub app_secret: String,
    pub query: QueryConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QueryConfig {
    #[serde(default = "default_days")]
    pub days: i64,
    #[serde(default)]
    pub end_date: String,
    #[serde(default = "default_tz")]
    pub timezone: String,
    /// 黑名单：要从「全部成员」中排除的 user_id（命中即排除）。留空配合 exclude_emails 也为空 => 统计全部。
    #[serde(default)]
    pub exclude_user_ids: Vec<i64>,
    /// 黑名单：要排除的成员 email（不区分大小写，命中即排除）。
    #[serde(default)]
    pub exclude_emails: Vec<String>,
    /// 自动拉取成员时的人数上限（接口单次统计上限 100）。
    #[serde(default = "default_max_users")]
    pub max_users: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeishuConfig {
    #[serde(default = "default_feishu_base")]
    pub base_url: String,
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub app_secret: String,
    /// 推送目标列表：可同时配置多个，群(chat_id)与个人(open_id/user_id/union_id/email)混合并存。
    #[serde(default)]
    pub targets: Vec<FeishuTarget>,
    #[serde(default)]
    pub webhook_url: String,
    #[serde(default)]
    pub webhook_secret: String,
    #[serde(default = "default_card_title")]
    pub card_title: String,
}

/// 单个飞书推送目标。
#[derive(Debug, Clone, Deserialize)]
pub struct FeishuTarget {
    /// chat_id（群）/ open_id / user_id / union_id / email / mobile（个人；mobile 会先换成 open_id）
    #[serde(rename = "type")]
    pub target_type: String,
    /// 与 type 对应的 ID：群用 oc_xxx；个人填 open_id(ou_xxx) / 邮箱等。
    pub id: String,
}

fn default_days() -> i64 {
    7
}
fn default_tz() -> String {
    "Asia/Shanghai".to_string()
}
fn default_max_users() -> usize {
    100
}
fn default_feishu_base() -> String {
    "https://open.feishu.cn".to_string()
}
fn default_card_title() -> String {
    "Trae AI 使用详情".to_string()
}

/// 解析后的统计时间窗口，同时提供日期串（YYYY-MM-DD）与秒级时间戳两种形态，
/// 以适配 user-metrics（按日期）与 user-model-usage（按时间戳）两类接口。
#[derive(Debug, Clone)]
pub struct DateRange {
    pub start_date: String,
    pub end_date: String,
    pub start_ts: i64,
    pub end_ts: i64,
    pub timezone: String,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("读取配置文件失败: {path}"))?;
        let cfg: Config =
            serde_yaml::from_str(&raw).with_context(|| format!("解析 YAML 失败: {path}"))?;
        Ok(cfg)
    }
}

impl QueryConfig {
    /// 根据配置推导统计窗口。显式日期优先，否则按 `days` 在 `timezone` 下回溯。
    pub fn resolve_date_range(&self) -> Result<DateRange> {
        let tz: Tz = self
            .timezone
            .parse()
            .map_err(|_| anyhow!("无法识别的时区: {}", self.timezone))?;

        let today = chrono::Utc::now().with_timezone(&tz).date_naive();

        // end_date 默认今天（可被配置覆盖）。
        let end = if self.end_date.trim().is_empty() {
            today
        } else {
            NaiveDate::parse_from_str(self.end_date.trim(), "%Y-%m-%d")
                .with_context(|| format!("end_date 格式应为 YYYY-MM-DD: {}", self.end_date))?
        };

        // 校验 days，避免 days<=0 静默塌缩成 1 天、或巨大 days 触发 Duration 溢出 panic；
        // 上限 180 天对应 user-model-usage 接口限制。
        if self.days < 1 {
            return Err(anyhow!("query.days 必须 >= 1，当前为 {}", self.days));
        }
        if self.days > MAX_RANGE_DAYS {
            return Err(anyhow!(
                "query.days 不能超过 {} 天（user-model-usage 限制最长 180 天），当前为 {}",
                MAX_RANGE_DAYS, self.days
            ));
        }

        // start_date 始终按 days 从 end_date 自动回溯：start = end - (days - 1)，含首尾共 days 天。
        let start = end - Duration::days(self.days - 1);

        let start_ts = local_midnight_ts(&tz, start, false)?;
        let end_ts = local_midnight_ts(&tz, end, true)?;

        Ok(DateRange {
            start_date: start.format("%Y-%m-%d").to_string(),
            end_date: end.format("%Y-%m-%d").to_string(),
            start_ts,
            end_ts,
            timezone: self.timezone.clone(),
        })
    }
}

/// 将某一天在指定时区下的「起始(00:00:00)」或「结束(23:59:59)」转为 Unix 秒级时间戳。
fn local_midnight_ts(tz: &Tz, date: NaiveDate, end_of_day: bool) -> Result<i64> {
    let time = if end_of_day {
        NaiveTime::from_hms_opt(23, 59, 59).unwrap()
    } else {
        NaiveTime::from_hms_opt(0, 0, 0).unwrap()
    };
    let naive = date.and_time(time);
    // DST 处理：歧义(fold)取较早的瞬间；若该本地时间落在 spring-forward 缺口中不存在，
    // 顺延 1 小时跨过缺口再取，避免合法日期被判为错误而中断整个流程。
    let mapped = tz.from_local_datetime(&naive);
    let dt = mapped
        .single()
        .or_else(|| mapped.earliest())
        .or_else(|| tz.from_local_datetime(&(naive + Duration::hours(1))).earliest())
        .ok_or_else(|| anyhow!("时区 {tz:?} 下无法确定本地时间: {naive}"))?;
    Ok(dt.timestamp())
}
