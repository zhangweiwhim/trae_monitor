//! 把统计数据组装成「飞书交互式卡片」(JSON 2.0)。
//!
//! 卡片正文包含可交互的 `chart` 组件（VChart 规范），用户可在飞书内
//! 悬停查看数值、按图例筛选系列，即所谓「可以操作的 chart」。

// 两个饼图（大模型调用分布 / 语言分布近似）暂时下线，相关函数保留待真实接口确定后再启用，
// 故放开 dead_code 以免未调用告警。
#![allow(dead_code)]

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::config::DateRange;
use crate::trae::{UsageReport, UserMetrics, UserModelUsage};

/// 取成员展示名：优先邮箱前缀，其次 user_id。
fn display_name(email: &str, user_id: i64) -> String {
    if !email.is_empty() {
        email.split('@').next().unwrap_or(email).to_string()
    } else {
        format!("uid:{user_id}")
    }
}

/// 堆叠柱状图：每位成员的 AI 提交行数 vs 非 AI 提交行数（按总提交行数从大到小）。
fn ai_contribution_chart(metrics: &[UserMetrics]) -> Value {
    let total = |m: &UserMetrics| m.metrics.ai_commit_lines + m.metrics.non_ai_commit_lines;
    let mut sorted: Vec<&UserMetrics> = metrics.iter().collect();
    sorted.sort_by(|a, b| total(b).cmp(&total(a)));

    let mut values = Vec::new();
    for m in sorted {
        let name = display_name(&m.email, m.user_id);
        values.push(json!({ "user": name, "type": "AI 提交行数", "value": m.metrics.ai_commit_lines }));
        values.push(json!({ "user": name, "type": "非 AI 提交行数", "value": m.metrics.non_ai_commit_lines }));
    }

    json!({
        "type": "bar",
        "title": { "text": "成员代码贡献（AI vs 非 AI 提交行数）" },
        "data": { "values": values },
        "xField": "user",
        "yField": "value",
        "seriesField": "type",
        "stack": true,
        "legends": [{ "visible": true, "orient": "top" }],
        "label": { "visible": false },
        "axes": [
            { "orient": "bottom", "label": { "visible": true } },
            { "orient": "left", "title": { "visible": true, "text": "代码行数" } }
        ]
    })
}

/// 柱状图：AI 生成率（%），按生成率从大到小。
fn ai_rate_chart(metrics: &[UserMetrics]) -> Value {
    let mut sorted: Vec<&UserMetrics> = metrics.iter().collect();
    sorted.sort_by(|a, b| {
        b.metrics
            .ai_generation_rate
            .partial_cmp(&a.metrics.ai_generation_rate)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let values: Vec<Value> = sorted
        .iter()
        .map(|m| json!({ "user": display_name(&m.email, m.user_id), "rate": m.metrics.ai_generation_rate }))
        .collect();

    json!({
        "type": "bar",
        "title": { "text": "成员 AI 代码生成率（%）" },
        "data": { "values": values },
        "xField": "user",
        "yField": "rate",
        "label": { "visible": true },
        "axes": [
            { "orient": "bottom", "label": { "visible": true } },
            { "orient": "left", "title": { "visible": true, "text": "生成率 %" } }
        ]
    })
}

/// 表格：每位成员一行，使用过的模型打平为逗号分隔，并汇总输入 / 输出 / 总 Token（按总量降序）。
/// 表格组件本身不支持放大，放大由下方配套的 token_usage_chart 提供。
fn token_usage_table(model_usage: &[UserModelUsage]) -> Option<Value> {
    let mut rows: Vec<(u64, Value)> = Vec::new();
    for u in model_usage {
        let mut models: Vec<String> = Vec::new();
        let mut input = 0u64;
        let mut output = 0u64;
        for mu in &u.model_usage {
            input += mu.usage.input_tokens;
            output += mu.usage.output_tokens;
            if !mu.model_name.is_empty() && !models.contains(&mu.model_name) {
                models.push(mu.model_name.clone());
            }
        }
        if input == 0 && output == 0 {
            continue;
        }
        rows.push((
            input + output,
            json!({
                "member": display_name(&u.email, 0),
                "models": models.join(", "),
                "input": input,
                "output": output,
                "total": input + output,
            }),
        ));
    }
    if rows.is_empty() {
        return None;
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    let rows: Vec<Value> = rows.into_iter().map(|(_, row)| row).collect();

    Some(json!({
        "tag": "table",
        "page_size": 10,
        "row_height": "low",
        "header_style": { "background_style": "grey", "text_align": "left", "bold": true, "lines": 1 },
        "columns": [
            { "name": "member", "display_name": "成员", "data_type": "text", "horizontal_align": "left", "width": "auto" },
            { "name": "models", "display_name": "使用模型", "data_type": "text", "horizontal_align": "left", "width": "auto" },
            { "name": "input", "display_name": "输入Token", "data_type": "number", "horizontal_align": "right", "width": "auto", "format": { "precision": 0, "separator": true } },
            { "name": "output", "display_name": "输出Token", "data_type": "number", "horizontal_align": "right", "width": "auto", "format": { "precision": 0, "separator": true } },
            { "name": "total", "display_name": "总Token", "data_type": "number", "horizontal_align": "right", "width": "auto", "format": { "precision": 0, "separator": true } }
        ],
        "rows": rows
    }))
}

/// 堆叠柱状图：每位成员的输入/输出 Token（堆叠高度=总量），按总量从大到小。
/// 用 chart 组件，飞书内可点击放大/全屏查看，作为上方明细表格的放大版可视化。
fn token_usage_chart(model_usage: &[UserModelUsage]) -> Option<Value> {
    // (member, input, output)
    let mut rows: Vec<(String, u64, u64)> = Vec::new();
    for u in model_usage {
        let mut input = 0u64;
        let mut output = 0u64;
        for mu in &u.model_usage {
            input += mu.usage.input_tokens;
            output += mu.usage.output_tokens;
        }
        if input == 0 && output == 0 {
            continue;
        }
        rows.push((display_name(&u.email, 0), input, output));
    }
    if rows.is_empty() {
        return None;
    }
    // 按总 Token 从大到小排序。
    rows.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)));

    let mut values = Vec::new();
    for (member, input, output) in rows {
        values.push(json!({ "member": member, "type": "输入Token", "value": input }));
        values.push(json!({ "member": member, "type": "输出Token", "value": output }));
    }

    Some(json!({
        "type": "bar",
        "title": { "text": "成员 Token 消耗（输入/输出 堆叠=总量）" },
        "data": { "values": values },
        "xField": "member",
        "yField": "value",
        "seriesField": "type",
        "stack": true,
        "legends": [{ "visible": true, "orient": "top" }],
        "axes": [
            { "orient": "bottom", "label": { "visible": true } },
            { "orient": "left", "title": { "visible": true, "text": "Token 数" } }
        ]
    }))
}

/// 把 (类别, 数值) 列表组装成饼图 spec：按数值降序，只取 Top N，其余归并到「其他」，
/// 以免类别过多导致飞书内图例溢出/被截断。
fn build_pie(title: &str, mut items: Vec<(String, u64)>, top_n: usize) -> Option<Value> {
    items.retain(|(_, v)| *v > 0);
    if items.is_empty() {
        return None;
    }
    items.sort_by(|a, b| b.1.cmp(&a.1));

    let mut values: Vec<Value> = Vec::new();
    for (name, v) in items.iter().take(top_n) {
        values.push(json!({ "name": name, "value": v }));
    }
    if items.len() > top_n {
        let others: u64 = items[top_n..].iter().map(|(_, v)| *v).sum();
        if others > 0 {
            values.push(json!({ "name": "其他", "value": others }));
        }
    }

    Some(json!({
        "type": "pie",
        "title": { "text": title },
        "data": { "values": values },
        "categoryField": "name",
        "valueField": "value",
        "outerRadius": 0.65,
        // 图例放右侧、纵向居中；类别已限制为 Top10，配合 4:3 比例可完整显示。
        "legends": [{ "visible": true, "orient": "right", "position": "middle" }],
        "label": { "visible": true, "position": "inside" },
        "tooltip": { "visible": true }
    }))
}

/// 饼图：全员大模型调用分布，标签为「模型名/来源」（如 auto/TRAE、claude-opus-4-8/anthropic）。
/// 注：Trae OpenAPI 仅提供 token 维度（无调用次数），此处按 Token 用量占比。
fn model_distribution_chart(model_usage: &[UserModelUsage]) -> Option<Value> {
    let mut by_model: BTreeMap<String, u64> = BTreeMap::new();
    for u in model_usage {
        for mu in &u.model_usage {
            let tokens = mu.usage.input_tokens + mu.usage.output_tokens;
            if tokens == 0 {
                continue;
            }
            let label = match (mu.model_name.trim(), mu.model_source.trim()) {
                ("", _) => "未知模型".to_string(),
                (name, "") => name.to_string(),
                (name, source) => format!("{name}/{source}"),
            };
            *by_model.entry(label).or_default() += tokens;
        }
    }
    build_pie("大模型调用分布图", by_model.into_iter().collect(), 10)
}

/// 饼图：AI 使用类型分布（按 model_type 的 Token 占比）。
/// 说明：Trae OpenAPI 未提供编程语言维度，此处以「模型使用类型」近似代替「编程语言分布」。
fn usage_type_distribution_chart(model_usage: &[UserModelUsage]) -> Option<Value> {
    let mut by_type: BTreeMap<String, u64> = BTreeMap::new();
    for u in model_usage {
        for mu in &u.model_usage {
            let tokens = mu.usage.input_tokens + mu.usage.output_tokens;
            if tokens == 0 {
                continue;
            }
            let label = match mu.model_type.as_str() {
                "CUE" => "CUE 代码补全",
                "Chat" => "Chat 对话",
                "" => "未知类型",
                other => other,
            };
            *by_type.entry(label.to_string()).or_default() += tokens;
        }
    }
    build_pie("AI 使用类型分布（近似语言分布）", by_type.into_iter().collect(), 10)
}

/// 折线图：全员每日 AI 生成代码行数趋势（按日期聚合）。
fn daily_trend_chart(metrics: &[UserMetrics]) -> Option<Value> {
    let mut by_date: BTreeMap<String, i64> = BTreeMap::new();
    for m in metrics {
        for d in &m.daily_trend {
            *by_date.entry(d.date.clone()).or_default() += d.metrics.ai_generated_code_lines;
        }
    }
    if by_date.len() < 2 {
        return None;
    }
    let values: Vec<Value> = by_date
        .into_iter()
        .map(|(date, lines)| json!({ "date": date, "value": lines }))
        .collect();

    Some(json!({
        "type": "line",
        "title": { "text": "每日 AI 生成代码行数趋势（全员合计）" },
        "data": { "values": values },
        "xField": "date",
        "yField": "value",
        "point": { "visible": true },
        "axes": [
            { "orient": "bottom", "label": { "visible": true } },
            { "orient": "left", "title": { "visible": true, "text": "代码行数" } }
        ]
    }))
}

fn chart_element(spec: Value) -> Value {
    json!({
        "tag": "chart",
        "aspect_ratio": "16:9",
        "color_theme": "rainbow",
        // preview=true：飞书内可点击图表放大/独立窗口查看（默认即开启，这里显式声明）。
        "preview": true,
        "chart_spec": spec
    })
}

/// 饼图专用元素：用接近正方形(4:3)的比例，避免 16:9 太扁把饼图/图例截断。
fn pie_element(spec: Value) -> Value {
    json!({
        "tag": "chart",
        "aspect_ratio": "4:3",
        "color_theme": "rainbow",
        "preview": true,
        "chart_spec": spec
    })
}

/// 顶部汇总文案。
fn summary_markdown(report: &UsageReport, range: Option<&DateRange>) -> Value {
    let period = range
        .map(|r| format!("{} ~ {}（{}）", r.start_date, r.end_date, r.timezone))
        .unwrap_or_else(|| "—".to_string());

    let total_ai_lines: i64 = report.user_metrics.iter().map(|m| m.metrics.ai_generated_code_lines).sum();
    let total_commit: i64 = report.user_metrics.iter().map(|m| m.metrics.total_commit_lines).sum();
    let a = &report.active_users;

    // 注意：活跃成员为企业全员口径，与上面「统计成员」（可能是配置筛选的子集）不是同一范围，措辞上区分清楚。
    let content = format!(
        "**统计周期**：{period}\n\
         **统计成员**：{} 人　**企业活跃成员（全员）**：{}（IDE {} / 插件 {} / CLI {}）\n\
         **AI 生成代码**：{} 行　**总提交**：{} 行",
        report.user_metrics.len(),
        a.total_active_users,
        a.ide_active_users,
        a.plugin_active_users,
        a.cli_active_users,
        total_ai_lines,
        total_commit,
    );

    json!({ "tag": "markdown", "content": content })
}

/// 往卡片中追加「分隔线 + 说明文案 + 图表/表格」一组元素。
fn add_section(elements: &mut Vec<Value>, desc: &str, element: Value) {
    elements.push(json!({ "tag": "hr" }));
    elements.push(json!({ "tag": "markdown", "content": format!("**📝 说明**：{desc}") }));
    elements.push(element);
}

/// 构建完整的飞书卡片 JSON（schema 2.0）。
pub fn build_card(report: &UsageReport, title: &str) -> Value {
    let range = report.date_range.as_ref();

    let mut elements = vec![summary_markdown(report, range)];

    // 展示全部在职成员（离职成员已在数据拉取阶段排除），AI 生成率为 0 的成员也照常显示。
    if !report.user_metrics.is_empty() {
        add_section(
            &mut elements,
            "各成员「AI 提交行数」与「非 AI 提交行数」对比，直观体现 AI 在实际提交代码中的占比。",
            chart_element(ai_contribution_chart(&report.user_metrics)),
        );
        add_section(
            &mut elements,
            "各成员的 AI 代码生成率：AI 生成的代码行数占其全部代码的百分比，越高代表越依赖 AI 辅助编码（含为 0 的成员）。",
            chart_element(ai_rate_chart(&report.user_metrics)),
        );
        if let Some(trend) = daily_trend_chart(&report.user_metrics) {
            add_section(
                &mut elements,
                "统计周期内全员每日 AI 生成代码行数的变化趋势，用于观察 AI 使用强度的走势。",
                chart_element(trend),
            );
        } else {
            // 单天窗口（days=1）只有 1 个日期，趋势折线无意义，显式提示而非默默不显示。
            elements.push(json!({ "tag": "hr" }));
            elements.push(json!({ "tag": "markdown", "content": "ℹ️ 「每日趋势」图需要至少 2 天数据，当前统计窗口为单天，故未展示（可调大 query.days）。" }));
        }
    } else {
        // user-metrics 未返回任何成员数据时，3 张相关图表都不会生成，给出明确原因，避免「图怎么没了」的困惑。
        elements.push(json!({ "tag": "hr" }));
        elements.push(json!({ "tag": "markdown", "content": "⚠️ 本次未取得成员 AI 用量数据，故「代码贡献 / 生成率 / 每日趋势」图未展示。可能原因：所选窗口内在职成员无数据，或 user-metrics 接口未返回。" }));
    }

    if let Some(table) = token_usage_table(&report.model_usage) {
        add_section(
            &mut elements,
            "各成员 Token 消耗明细：使用模型（逗号分隔）、输入 / 输出 / 总 Token，按总量从大到小，用于成本核算。",
            table,
        );
    }

    if let Some(chart) = token_usage_chart(&report.model_usage) {
        add_section(
            &mut elements,
            "上方 Token 明细的可视化版本（输入/输出 堆叠，柱高=总量），飞书表格不支持放大，看大图请点此图右上角放大。",
            chart_element(chart),
        );
    }

    // 两个饼图（大模型调用分布 / 编程语言分布·近似）暂时下线：
    // 接口未提供「调用次数」与「编程语言」维度，待拿到真实数据接口后再启用。
    // 启用方式：取消下方注释即可（函数 model_distribution_chart / usage_type_distribution_chart 已保留）。
    // if let Some(pie) = model_distribution_chart(&report.model_usage) {
    //     add_section(&mut elements, "大模型调用分布：标签「模型名/来源」，按 Token 用量占比，Top10。", pie_element(pie));
    // }
    // if let Some(pie) = usage_type_distribution_chart(&report.model_usage) {
    //     add_section(&mut elements, "编程语言分布（近似）：以模型使用类型 (CUE/Chat) Token 占比代替。", pie_element(pie));
    // }

    let subtitle = range
        .map(|r| format!("{} ~ {}", r.start_date, r.end_date))
        .unwrap_or_default();

    json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true, "update_multi": true },
        "header": {
            "title": { "tag": "plain_text", "content": title },
            "subtitle": { "tag": "plain_text", "content": subtitle },
            "template": "blue"
        },
        "body": { "elements": elements }
    })
}
