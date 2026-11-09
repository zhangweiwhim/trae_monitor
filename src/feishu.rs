//! 飞书（Lark）推送客户端。
//!
//! 支持两种通道：
//!   1. 自建应用 API：换取 tenant_access_token 后调用 im/v1/messages，
//!      可发到「群(chat_id)」或「个人(open_id/user_id/union_id/email)」。
//!   2. 群自定义机器人 Webhook：直接 POST 卡片，仅能发到机器人所在群，
//!      支持可选的 HMAC-SHA256 签名校验。

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;

use crate::config::{FeishuConfig, FeishuTarget};

type HmacSha256 = Hmac<Sha256>;

pub struct FeishuClient {
    http: reqwest::Client,
    cfg: FeishuConfig,
}

#[derive(Debug, Deserialize)]
struct TenantTokenResp {
    code: i64,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    tenant_access_token: String,
}

#[derive(Debug, Deserialize)]
struct SendResp {
    code: i64,
    #[serde(default)]
    msg: String,
}

impl FeishuClient {
    pub fn new(cfg: FeishuConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("构建飞书 HTTP 客户端失败")?;
        Ok(Self { http, cfg })
    }

    fn base(&self) -> &str {
        self.cfg.base_url.trim_end_matches('/')
    }

    /// 根据配置选择通道并发送卡片。
    pub async fn send_card(&self, card: &Value) -> Result<()> {
        if !self.cfg.webhook_url.trim().is_empty() {
            self.send_via_webhook(card).await
        } else {
            self.send_via_app(card).await
        }
    }

    // ---------------------- 自建应用 API 通道 ----------------------

    async fn tenant_access_token(&self) -> Result<String> {
        let url = format!("{}/open-apis/auth/v3/tenant_access_token/internal", self.base());
        let resp = self
            .http
            .post(&url)
            .json(&json!({ "app_id": self.cfg.app_id, "app_secret": self.cfg.app_secret }))
            .send()
            .await
            .context("请求飞书 tenant_access_token 失败")?;
        let body: TenantTokenResp = resp.json().await.context("tenant_access_token 响应解析失败")?;
        if body.code != 0 || body.tenant_access_token.is_empty() {
            return Err(anyhow!("获取 tenant_access_token 失败 code={} msg={}", body.code, body.msg));
        }
        Ok(body.tenant_access_token)
    }

    async fn send_via_app(&self, card: &Value) -> Result<()> {
        if self.cfg.targets.is_empty() {
            return Err(anyhow!("feishu.targets 为空，且未配置 webhook_url，没有可推送的目标"));
        }
        // tenant_access_token 取一次，复用于所有目标。
        let token = self.tenant_access_token().await?;
        let url = format!("{}/open-apis/im/v1/messages", self.base());
        // content 必须是「卡片 JSON 的字符串」，所有目标共用同一份。
        let content = serde_json::to_string(card).context("序列化卡片失败")?;

        // 逐个目标推送：单个失败不影响其它目标，最终只要有一个成功即视为成功。
        let (mut ok, mut fail) = (0usize, 0usize);
        for t in &self.cfg.targets {
            if t.id.trim().is_empty() {
                tracing::warn!("跳过 id 为空的推送目标 (type={})", t.target_type);
                fail += 1;
                continue;
            }
            // 手机号无法直接作为 receive_id，先换成 open_id 再发送。
            let resolved;
            let t = if t.target_type == "mobile" {
                match self.mobile_to_open_id(&token, t.id.trim()).await {
                    Ok(open_id) => {
                        resolved = FeishuTarget { target_type: "open_id".into(), id: open_id };
                        &resolved
                    }
                    Err(e) => {
                        fail += 1;
                        tracing::warn!("手机号 {} 换取 open_id 失败：{e:#}", t.id);
                        continue;
                    }
                }
            } else {
                t
            };
            match self.send_one(&url, &token, t, &content).await {
                Ok(message_id) => {
                    ok += 1;
                    tracing::info!("飞书卡片已发送 → {}:{} message_id={}", t.target_type, t.id, message_id);
                }
                Err(e) => {
                    fail += 1;
                    tracing::warn!("发送到 {}:{} 失败：{e:#}", t.target_type, t.id);
                }
            }
        }
        if ok == 0 {
            return Err(anyhow!("全部 {} 个推送目标均发送失败", fail));
        }
        tracing::info!("飞书推送完成：成功 {} 个，失败 {} 个", ok, fail);
        Ok(())
    }

    /// 通过 contact/v3/users/batch_get_id 将手机号换成 open_id。
    /// 需开通权限：contact:user.id:readonly（通过手机号或邮箱获取用户 ID）。
    async fn mobile_to_open_id(&self, token: &str, mobile: &str) -> Result<String> {
        let url = format!("{}/open-apis/contact/v3/users/batch_get_id", self.base());
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .query(&[("user_id_type", "open_id")])
            .json(&json!({ "mobiles": [mobile] }))
            .send()
            .await
            .context("调用 contact/v3/users/batch_get_id 失败")?;
        let status = resp.status();
        let body: Value = resp.json().await.context("batch_get_id 响应解析失败")?;
        if !status.is_success() {
            return Err(anyhow!("HTTP {status}: {body}"));
        }
        parse_batch_get_id(&body)
    }

    /// 向单个目标发送一张交互式卡片，返回 message_id。
    async fn send_one(
        &self,
        url: &str,
        token: &str,
        target: &FeishuTarget,
        content: &str,
    ) -> Result<String> {
        let payload = json!({
            "receive_id": target.id,
            "msg_type": "interactive",
            "content": content,
        });
        let resp = self
            .http
            .post(url)
            .bearer_auth(token)
            .query(&[("receive_id_type", target.target_type.as_str())])
            .json(&payload)
            .send()
            .await
            .context("调用 im/v1/messages 失败")?;

        let status = resp.status();
        let body: Value = resp.json().await.context("im/v1/messages 响应解析失败")?;
        let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if !status.is_success() || code != 0 {
            return Err(anyhow!("HTTP {status}: {body}"));
        }
        Ok(body
            .get("data")
            .and_then(|d| d.get("message_id"))
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_string())
    }

    // ---------------------- 群自定义机器人 Webhook 通道 ----------------------

    fn gen_sign(secret: &str, timestamp: i64) -> Result<String> {
        // string_to_sign = "{timestamp}\n{secret}"，作为 HMAC 密钥，对空消息求 HMAC-SHA256。
        let string_to_sign = format!("{timestamp}\n{secret}");
        let mac = HmacSha256::new_from_slice(string_to_sign.as_bytes())
            .map_err(|e| anyhow!("初始化 HMAC 失败: {e}"))?;
        Ok(STANDARD.encode(mac.finalize().into_bytes()))
    }

    async fn send_via_webhook(&self, card: &Value) -> Result<()> {
        let mut payload = json!({ "msg_type": "interactive", "card": card });

        if !self.cfg.webhook_secret.trim().is_empty() {
            let ts = chrono::Utc::now().timestamp();
            let sign = Self::gen_sign(self.cfg.webhook_secret.trim(), ts)?;
            payload["timestamp"] = json!(ts.to_string());
            payload["sign"] = json!(sign);
        }

        let resp = self
            .http
            .post(self.cfg.webhook_url.trim())
            .json(&payload)
            .send()
            .await
            .context("调用飞书 Webhook 失败")?;

        let status = resp.status();
        let body: SendResp = resp.json().await.context("Webhook 响应解析失败")?;
        if !status.is_success() || body.code != 0 {
            return Err(anyhow!("飞书 Webhook 发送失败 HTTP {status} code={} msg={}", body.code, body.msg));
        }
        tracing::info!("飞书卡片已发送（Webhook）");
        Ok(())
    }
}

/// 从 batch_get_id 响应中取出第一个用户的 open_id；
/// 手机号不存在或应用通讯录权限范围不含该用户时，飞书不返回 user_id，此时报错。
fn parse_batch_get_id(body: &Value) -> Result<String> {
    let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        return Err(anyhow!("batch_get_id 返回错误：{body}"));
    }
    body.pointer("/data/user_list/0/user_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("未找到该手机号对应的用户（或不在应用通讯录权限范围内）：{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_batch_get_id_returns_open_id() {
        let body = json!({"code":0,"data":{"user_list":[{"mobile":"13800000000","user_id":"ou_abc"}]}});
        assert_eq!(parse_batch_get_id(&body).unwrap(), "ou_abc");
    }

    #[test]
    fn parse_batch_get_id_errors_when_user_not_found() {
        // 找不到用户时飞书仍返回 code=0，但没有 user_id；不能把空值当成功发出去。
        let body = json!({"code":0,"data":{"user_list":[{"mobile":"13800000000"}]}});
        assert!(parse_batch_get_id(&body).is_err());
    }

    #[test]
    fn parse_batch_get_id_errors_on_api_error() {
        let body = json!({"code":99991672,"msg":"Access denied"});
        assert!(parse_batch_get_id(&body).is_err());
    }
}
