//! 上游模型列表探测。
//!
//! 为什么需要它：手工维护模型清单必然过期。上游加了新模型，用户得手动去每个
//! 渠道补一遍 —— 这正是 new-api 用起来累的地方。这里让「拉取上游模型列表」
//! 成为一等操作，UI 上一键同步。
//!
//! 四家的列表接口形状不同，但都能归一成「一串模型 ID」。归一化在这里做，
//! 而不是留给调用方 `match` 协议 —— 那样每个调用点都要重复一次。

use refract_core::{Credential, ErrorKind, GatewayError, Protocol, UpstreamAddress};
use serde_json::Value;

use crate::client::{UpstreamClient, UpstreamRequest};

/// 一个探测到的模型。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ModelProbe {
    /// 模型 ID，可直接用于请求。
    pub id: String,
    /// 上游给的展示名，可能与 ID 不同（Gemini 就是两者分离）。
    pub display_name: Option<String>,
}

impl ModelProbe {
    fn bare(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: None,
        }
    }
}

/// 拉取并归一化上游模型列表。
pub async fn probe_models(
    client: &UpstreamClient,
    protocol: Protocol,
    address: &UpstreamAddress,
    credential: &Credential,
    proxy: Option<&str>,
) -> Result<Vec<ModelProbe>, GatewayError> {
    let mut request = UpstreamRequest::list_models(protocol, address, credential);
    request.proxy = proxy;
    let response = client.send(request).await?;
    parse_model_list(protocol, &response.body)
}

/// 查询 OpenAI 兼容上游的剩余余额（美元或站点自定币种）。
///
/// 走 `/v1/dashboard/billing/subscription`（额度上限）与
/// `/v1/dashboard/billing/usage`（本月已用，单位美分）——OpenAI 官方早已
/// 废弃这对端点，但它们是中转站（one-api/new-api 系）的事实标准余额协议。
/// 只对 OpenAI 形状协议有意义；其他协议返回配置错误。
pub async fn probe_balance(
    client: &UpstreamClient,
    protocol: Protocol,
    address: &UpstreamAddress,
    credential: &Credential,
    proxy: Option<&str>,
) -> Result<f64, GatewayError> {
    if !matches!(protocol, Protocol::Chat | Protocol::Responses) {
        return Err(GatewayError::new(
            ErrorKind::Configuration,
            "balance probing is only defined for OpenAI-shaped upstreams",
        ));
    }

    // 复用模型列表的地址解析拿到 `{base}{prefix}/models`，再替换端点段 ——
    // 这样自定义 base/前缀（中转站带路径前缀是常态）自然生效。
    let models_url = address
        .resolve(protocol, refract_core::Action::ListModels, "")
        .map_err(|e| GatewayError::new(ErrorKind::Configuration, e.to_string()))?;
    let base = models_url
        .as_str()
        .trim_end_matches('/')
        .trim_end_matches("/models")
        .to_owned();

    let subscription: Value = fetch_billing_json(
        client,
        &format!("{base}/dashboard/billing/subscription"),
        credential,
        proxy,
    )
    .await?;
    let hard_limit = subscription
        .get("hard_limit_usd")
        .and_then(Value::as_f64)
        .ok_or_else(|| {
            GatewayError::new(
                ErrorKind::UpstreamError,
                "upstream did not report hard_limit_usd — balance API unsupported",
            )
        })?;

    // 本月用量窗口。end_date 用明天，避免时区导致今天的消耗被漏掉。
    let now = chrono::Utc::now();
    let start = now.format("%Y-%m-01").to_string();
    let end = (now + chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let usage: Value = fetch_billing_json(
        client,
        &format!("{base}/dashboard/billing/usage?start_date={start}&end_date={end}"),
        credential,
        proxy,
    )
    .await?;
    let used_cents = usage
        .get("total_usage")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);

    Ok(hard_limit - used_cents / 100.0)
}

async fn fetch_billing_json(
    client: &UpstreamClient,
    url: &str,
    credential: &Credential,
    proxy: Option<&str>,
) -> Result<Value, GatewayError> {
    let response = client.get_json(url, credential, proxy).await?;
    Ok(response)
}

/// 把各家的列表响应归一成模型数组。
///
/// 公开出来是为了能脱离网络单测 —— 归一化逻辑的分支比 HTTP 部分多得多，
/// 让它依赖 mock server 才能测是设计失误。
pub fn parse_model_list(protocol: Protocol, body: &Value) -> Result<Vec<ModelProbe>, GatewayError> {
    let models = match protocol {
        // OpenAI 系（Chat / Responses 共用 /v1/models）：{"data":[{"id":"gpt-4o"}]}
        Protocol::Chat | Protocol::Responses => from_openai(body),
        // Anthropic：{"data":[{"id":"claude-...","display_name":"Claude ..."}]}
        Protocol::Messages => from_anthropic(body),
        // Gemini：{"models":[{"name":"models/gemini-2.5-pro","displayName":"..."}]}
        Protocol::Gemini => from_gemini(body),
    };

    // 空列表当错误报：能连上但一个模型都没有，几乎总是地址或密钥配错，
    // 静默返回空数组会让用户以为「同步成功了但上游没模型」。
    if models.is_empty() {
        return Err(GatewayError::new(
            ErrorKind::UpstreamError,
            "upstream returned no models; check the base URL and API key",
        )
        .with_protocol(protocol));
    }

    Ok(dedup_preserving_order(models))
}

fn from_openai(body: &Value) -> Vec<ModelProbe> {
    // 中转站有时直接返回裸数组而不是 {"data": [...]}。两种都收。
    let items = body
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| body.as_array());

    items
        .into_iter()
        .flatten()
        .filter_map(|item| {
            // 元素可能是对象，也可能就是一个字符串。
            if let Some(id) = item.as_str() {
                return Some(ModelProbe::bare(id));
            }
            let id = item.get("id").and_then(Value::as_str)?;
            Some(ModelProbe {
                id: id.to_owned(),
                display_name: item
                    .get("display_name")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .filter(|m| !m.id.trim().is_empty())
        .collect()
}

fn from_anthropic(body: &Value) -> Vec<ModelProbe> {
    body.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item.get("id").and_then(Value::as_str)?;
            Some(ModelProbe {
                id: id.to_owned(),
                display_name: item
                    .get("display_name")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .filter(|m| !m.id.trim().is_empty())
        .collect()
}

fn from_gemini(body: &Value) -> Vec<ModelProbe> {
    body.get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let raw = item.get("name").and_then(Value::as_str)?;
            // Gemini 的 name 是 `models/gemini-2.5-pro`，但请求时用的是
            // 去掉前缀的部分。存带前缀的值会让后续请求 404。
            let id = raw.strip_prefix("models/").unwrap_or(raw);
            Some(ModelProbe {
                id: id.to_owned(),
                display_name: item
                    .get("displayName")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .filter(|m| !m.id.trim().is_empty())
        .collect()
}

/// 去重但保留上游给的顺序。
///
/// 上游的顺序通常有意义（新模型在前），排序会打乱它。用 `Vec::contains` 而非
/// `HashSet`：列表长度是几十量级，线性查找比建哈希表更快也更省。
fn dedup_preserving_order(models: Vec<ModelProbe>) -> Vec<ModelProbe> {
    let mut seen: Vec<&str> = Vec::with_capacity(models.len());
    let mut out = Vec::with_capacity(models.len());
    for model in &models {
        if !seen.contains(&model.id.as_str()) {
            seen.push(&model.id);
            out.push(model.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn ids(models: &[ModelProbe]) -> Vec<&str> {
        models.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn openai_list_is_parsed() {
        let body = json!({
            "object": "list",
            "data": [
                {"id": "gpt-5", "object": "model"},
                {"id": "gpt-4o", "object": "model"}
            ]
        });
        let models = parse_model_list(Protocol::Chat, &body).unwrap();
        assert_eq!(ids(&models), vec!["gpt-5", "gpt-4o"]);
    }

    #[test]
    fn responses_protocol_shares_the_openai_shape() {
        let body = json!({"data": [{"id": "o3"}]});
        assert_eq!(
            ids(&parse_model_list(Protocol::Responses, &body).unwrap()),
            vec!["o3"]
        );
    }

    #[test]
    fn relay_returning_a_bare_array_still_works() {
        let body = json!([{"id": "glm-4.6"}, {"id": "kimi-k2"}]);
        let models = parse_model_list(Protocol::Chat, &body).unwrap();
        assert_eq!(ids(&models), vec!["glm-4.6", "kimi-k2"]);
    }

    #[test]
    fn relay_returning_bare_strings_still_works() {
        let body = json!({"data": ["deepseek-v3", "deepseek-r1"]});
        let models = parse_model_list(Protocol::Chat, &body).unwrap();
        assert_eq!(ids(&models), vec!["deepseek-v3", "deepseek-r1"]);
    }

    #[test]
    fn anthropic_list_keeps_display_names() {
        let body = json!({
            "data": [
                {"id": "claude-sonnet-4-6", "display_name": "Claude Sonnet 4.6"}
            ]
        });
        let models = parse_model_list(Protocol::Messages, &body).unwrap();
        assert_eq!(models[0].id, "claude-sonnet-4-6");
        assert_eq!(models[0].display_name.as_deref(), Some("Claude Sonnet 4.6"));
    }

    #[test]
    fn gemini_strips_the_models_prefix() {
        let body = json!({
            "models": [
                {"name": "models/gemini-2.5-pro", "displayName": "Gemini 2.5 Pro"},
                {"name": "models/gemini-2.5-flash"}
            ]
        });
        let models = parse_model_list(Protocol::Gemini, &body).unwrap();
        // 带 `models/` 前缀去请求会 404 —— 前缀必须剥掉。
        assert_eq!(ids(&models), vec!["gemini-2.5-pro", "gemini-2.5-flash"]);
        assert_eq!(models[0].display_name.as_deref(), Some("Gemini 2.5 Pro"));
    }

    #[test]
    fn gemini_name_without_prefix_is_accepted() {
        let body = json!({"models": [{"name": "gemini-2.5-pro"}]});
        assert_eq!(
            ids(&parse_model_list(Protocol::Gemini, &body).unwrap()),
            vec!["gemini-2.5-pro"]
        );
    }

    #[test]
    fn duplicates_are_removed_keeping_first_occurrence() {
        let body = json!({"data": [{"id": "a"}, {"id": "b"}, {"id": "a"}]});
        let models = parse_model_list(Protocol::Chat, &body).unwrap();
        assert_eq!(ids(&models), vec!["a", "b"]);
    }

    #[test]
    fn upstream_order_is_preserved_not_sorted() {
        let body = json!({"data": [{"id": "z"}, {"id": "a"}, {"id": "m"}]});
        let models = parse_model_list(Protocol::Chat, &body).unwrap();
        assert_eq!(ids(&models), vec!["z", "a", "m"]);
    }

    #[test]
    fn entries_without_usable_ids_are_dropped() {
        let body = json!({"data": [{"object": "model"}, {"id": "  "}, {"id": "ok"}]});
        let models = parse_model_list(Protocol::Chat, &body).unwrap();
        assert_eq!(ids(&models), vec!["ok"]);
    }

    #[test]
    fn empty_list_is_an_error_not_success() {
        let err = parse_model_list(Protocol::Chat, &json!({"data": []})).unwrap_err();
        assert_eq!(err.kind, ErrorKind::UpstreamError);
        assert!(err.message.contains("no models"));
    }

    #[test]
    fn wrong_shape_is_an_error() {
        // Gemini 的响应打到 Chat 解析器上应当报错，而不是静默返回空。
        let body = json!({"models": [{"name": "models/gemini-2.5-pro"}]});
        assert!(parse_model_list(Protocol::Chat, &body).is_err());
    }

    #[test]
    fn null_body_is_an_error() {
        assert!(parse_model_list(Protocol::Chat, &Value::Null).is_err());
    }
}
