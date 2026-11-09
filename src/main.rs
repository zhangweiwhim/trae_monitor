//! Trae 企业版用量监控 → 飞书图表卡片推送。
//!
//! 流程：加载配置 → Trae 鉴权 → 确定统计成员 → 拉取(活跃数/AI用量/模型用量)
//!       → 组装可交互图表卡片 → 推送到飞书群或个人。
//!
//! 用法：
//!   trae-usage-monitor [--config ./config.yaml] [--dry-run]
//!     --config   指定配置文件路径（默认 ./config.yaml）
//!     --dry-run  只打印将要发送的卡片 JSON，不真正推送

mod chart;
mod config;
mod feishu;
mod trae;

use std::collections::HashSet;

use anyhow::{Context, Result};

use config::Config;
use feishu::FeishuClient;
use trae::{TraeClient, UsageReport, MAX_BATCH_USERS};

struct Args {
    config_path: String,
    dry_run: bool,
    sample: bool,
}

fn parse_args() -> Args {
    let mut config_path = "config.yaml".to_string();
    let mut dry_run = false;
    let mut sample = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" | "-c" => {
                if let Some(p) = it.next() {
                    config_path = p;
                }
            }
            "--dry-run" => dry_run = true,
            // 用内置样例数据离线预览卡片 JSON，无需配置/网络。
            "--sample" => {
                sample = true;
                dry_run = true;
            }
            other => eprintln!("忽略未知参数: {other}"),
        }
    }
    Args { config_path, dry_run, sample }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = parse_args();

    // --sample：用内置样例数据直接预览图表卡片，不读配置、不联网。
    if args.sample {
        let report = sample_report();
        let card = chart::build_card(&report, "Trae AI 使用详情周报（样例）");
        println!("{}", serde_json::to_string_pretty(&card)?);
        return Ok(());
    }

    let cfg = Config::load(&args.config_path)
        .with_context(|| format!("加载配置失败: {}", args.config_path))?;

    let report = collect_usage(&cfg).await.context("拉取 Trae 用量数据失败")?;
    let card = chart::build_card(&report, &cfg.feishu.card_title);

    if args.dry_run {
        println!("{}", serde_json::to_string_pretty(&card)?);
        tracing::info!("--dry-run：已打印卡片 JSON，未推送飞书");
        return Ok(());
    }

    let feishu = FeishuClient::new(cfg.feishu.clone())?;
    feishu.send_card(&card).await.context("推送飞书卡片失败")?;
    tracing::info!("完成");
    Ok(())
}

/// 判断成员是否在职：status == "active" 视为在职；status 为空（接口未提供该字段）按在职处理以免误删；
/// 其它值（如 inactive / removed / disabled 等）一律视为已不在职，不纳入统计。
fn is_in_service(status: &str) -> bool {
    let s = status.trim().to_lowercase();
    s.is_empty() || s == "active"
}

/// 调用 Trae 接口，汇总成一个 UsageReport。
async fn collect_usage(cfg: &Config) -> Result<UsageReport> {
    let q = &cfg.trae.query;
    let range = q.resolve_date_range()?;
    tracing::info!("统计窗口 {} ~ {} ({})", range.start_date, range.end_date, range.timezone);

    let client = TraeClient::new(cfg.trae.clone())?;
    client.authenticate().await?;

    // 1) 始终通过接口自动获取全部成员，再按配置黑名单排除（黑名单为空则统计全部）。
    let users = match client.list_users(q.max_users).await {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("成员列表获取失败，将无法统计成员用量：{e:#}");
            Vec::new()
        }
    };
    let total = users.len();

    // 排除「已不在职」的成员：只保留 status 为 active 或接口未提供 status 的成员。
    let in_service: Vec<&trae::User> = users.iter().filter(|u| is_in_service(&u.status)).collect();
    let departed = total.saturating_sub(in_service.len());

    // 黑名单：user_id 或 email 命中任一即排除；email 不区分大小写。
    let exclude_ids: HashSet<i64> = q.exclude_user_ids.iter().copied().collect();
    let exclude_emails: HashSet<String> =
        q.exclude_emails.iter().map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()).collect();

    let mut user_ids: Vec<i64> = in_service
        .iter()
        .filter(|u| u.user_id != 0) // 过滤无效占位 ID
        .filter(|u| !exclude_ids.contains(&u.user_id))
        .filter(|u| !exclude_emails.contains(&u.email.trim().to_lowercase()))
        .map(|u| u.user_id)
        .collect();
    let blacklisted = in_service.len().saturating_sub(user_ids.len());
    tracing::info!(
        "自动拉取 {} 名成员：离职/停用排除 {} 名，黑名单排除 {} 名，最终统计 {} 名",
        total, departed, blacklisted, user_ids.len()
    );

    // user-metrics / user-model-usage 单次最多 100 人，超出部分明确告警后截断。
    if user_ids.len() > MAX_BATCH_USERS {
        tracing::warn!("成员数 {} 超过单次上限 {}，仅统计前 {} 人", user_ids.len(), MAX_BATCH_USERS, MAX_BATCH_USERS);
        user_ids.truncate(MAX_BATCH_USERS);
    }
    let has_targets = !user_ids.is_empty();
    // 统一以 user_id 调用统计接口，emails 仅用于占位（user_id 优先）。
    let emails: Vec<String> = Vec::new();

    let mut report = UsageReport { date_range: Some(range.clone()), ..Default::default() };

    // 2) 活跃成员数（企业全员口径，失败不致命）。
    match client.active_users(&range).await {
        Ok(a) => report.active_users = a,
        Err(e) => tracing::warn!("活跃成员数获取失败：{e:#}"),
    }

    if has_targets {
        // 3) 成员 AI 用量（失败不致命：仍发送含活跃数/模型用量的卡片，避免整体放弃推送）。
        match client.user_metrics(&range, &user_ids, &emails).await {
            Ok(m) => {
                tracing::info!("获取到 {} 名成员的 AI 用量", m.len());
                report.user_metrics = m;
            }
            Err(e) => tracing::warn!("成员 AI 用量获取失败：{e:#}"),
        }

        // 4) 成员模型 token 用量（失败不致命）。
        match client.user_model_usage(&range, &user_ids, &emails).await {
            Ok(m) => {
                tracing::info!("获取到 {} 名成员的模型用量", m.len());
                report.model_usage = m;
            }
            Err(e) => tracing::warn!("模型用量获取失败：{e:#}"),
        }
    } else {
        tracing::warn!("没有可统计的成员（成员列表为空或全部被排除），跳过 AI 用量与模型用量查询");
    }

    Ok(report)
}

/// 构造一份样例报告，用于 `--sample` 离线预览图表卡片。
fn sample_report() -> UsageReport {
    use config::DateRange;
    use trae::{ActiveUsers, DailyMetrics, Metrics, ModelUsage, TokenUsage, UserMetrics, UserModelUsage};

    let mk = |email: &str, uid: i64, ai_lines: i64, ai_commit: i64, non_ai: i64, rate: f64, trend: &[(&str, i64)]| UserMetrics {
        user_id: uid,
        email: email.to_string(),
        metrics: Metrics {
            ai_generation_rate: rate,
            ai_contribution_rate: rate * 0.6,
            ai_generated_code_lines: ai_lines,
            ai_commit_lines: ai_commit,
            non_ai_commit_lines: non_ai,
            total_commit_lines: ai_commit + non_ai,
            ..Default::default()
        },
        daily_trend: trend
            .iter()
            .map(|(d, v)| DailyMetrics {
                date: d.to_string(),
                metrics: Metrics { ai_generated_code_lines: *v, ..Default::default() },
            })
            .collect(),
    };

    UsageReport {
        date_range: Some(DateRange {
            start_date: "2026-06-05".into(),
            end_date: "2026-06-11".into(),
            start_ts: 0,
            end_ts: 0,
            timezone: "Asia/Shanghai".into(),
        }),
        active_users: ActiveUsers {
            total_active_users: 3,
            ide_active_users: 2,
            plugin_active_users: 2,
            cli_active_users: 1,
        },
        user_metrics: vec![
            mk("zhangsan@example.com", 1001, 12000, 9000, 3000, 42.5,
               &[("2026-06-09", 3500), ("2026-06-10", 4200), ("2026-06-11", 4300)]),
            mk("lisi@example.com", 1002, 8200, 6000, 5000, 33.1,
               &[("2026-06-09", 2600), ("2026-06-10", 2800), ("2026-06-11", 2800)]),
            mk("wangwu@example.com", 1003, 5400, 3000, 7000, 21.0,
               &[("2026-06-09", 1700), ("2026-06-10", 1800), ("2026-06-11", 1900)]),
        ],
        model_usage: vec![
            UserModelUsage {
                email: "zhangsan@example.com".into(),
                model_usage: vec![
                    ModelUsage { model_name: "Doubao-Seed-Code".into(), model_type: "Chat".into(), model_source: "Trae".into(), usage: TokenUsage { input_tokens: 320_000, output_tokens: 980_000 } },
                    ModelUsage { model_name: "Doubao-Seed-2.0-Code".into(), model_type: "CUE".into(), model_source: "Trae".into(), usage: TokenUsage { input_tokens: 120_000, output_tokens: 60_000 } },
                ],
            },
            UserModelUsage {
                email: "lisi@example.com".into(),
                model_usage: vec![
                    ModelUsage { model_name: "Doubao-Seed-Code".into(), model_type: "Chat".into(), model_source: "Trae".into(), usage: TokenUsage { input_tokens: 210_000, output_tokens: 540_000 } },
                ],
            },
        ],
    }
}
