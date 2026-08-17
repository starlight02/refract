//! 统一中间表示（IR）。
//!
//! 四种协议通过 IR 互转，而非两两点对点转换 —— 后者是 O(n²) 的工作量，
//! 且每加一个协议要动所有已有代码。
//!
//! # 设计原则
//!
//! IR 是四种协议的**并集**而非交集。任何一个协议能表达但 IR 表达不了的东西，
//! 都会在转换中丢失，所以 IR 宁可冗余也不能缺失。确实无法归一化的字段进
//! [`Extensions`]，由目标编码器决定是丢弃还是尽力还原。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 无法归一化的协议专属字段。
///
/// 键为 `"<protocol>.<field>"`，如 `"gemini.safetySettings"`、
/// `"responses.previous_response_id"`。这样同一个 IR 可以同时携带多个协议的
/// 专属数据而不冲突，且编码器能精确地只取自己那部分。
pub type Extensions = BTreeMap<String, Value>;

/// 消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// 系统/开发者指令。归一化后一般会被提到 [`UnifiedRequest::system`]。
    System,
    /// 用户。
    User,
    /// 模型。Gemini 里叫 `model`，其余协议叫 `assistant`。
    Assistant,
    /// 工具执行结果。Anthropic/Gemini 把它塞进 user 消息的 content block，
    /// OpenAI Chat 用独立的 `role: "tool"` 消息。
    Tool,
}

/// 媒体来源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaSource {
    /// 远程 URL。
    Url(String),
    /// Base64 内联数据（不含 data URI 前缀）。
    Base64(String),
    /// 上游文件 ID（OpenAI `file_id` / Gemini `fileUri`）。
    FileId(String),
    /// 纯文本（Anthropic `source.type == "text"`）。**不是** base64，
    /// 内容本身就是原文；跨协议转码时才按需 base64 编码。
    Text(String),
}

impl MediaSource {
    /// 从可能是 data URI 的字符串解析。
    ///
    /// `data:image/png;base64,iVBOR...` → `(Base64("iVBOR..."), Some("image/png"))`
    pub fn parse_data_uri(raw: &str) -> (Self, Option<String>) {
        if let Some(rest) = raw.strip_prefix("data:")
            && let Some((meta, payload)) = rest.split_once(',')
        {
            let mime = meta
                .split(';')
                .next()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            if meta.contains("base64") {
                return (MediaSource::Base64(payload.to_owned()), mime);
            }
        }
        (MediaSource::Url(raw.to_owned()), None)
    }

    /// 渲染成 data URI（Base64/Text 变体）或原样 URL。
    pub fn to_data_uri(&self, mime: Option<&str>) -> String {
        match self {
            MediaSource::Url(u) => u.clone(),
            MediaSource::Base64(data) => {
                let mime = mime.unwrap_or("application/octet-stream");
                format!("data:{mime};base64,{data}")
            }
            MediaSource::FileId(id) => id.clone(),
            // 纯文本跨协议时按 base64 data URI 表达，语义等价。
            MediaSource::Text(text) => {
                use base64::Engine as _;
                let mime = mime.unwrap_or("text/plain");
                let data = base64::engine::general_purpose::STANDARD.encode(text);
                format!("data:{mime};base64,{data}")
            }
        }
    }
}

/// 内容片段。
///
/// 这是 IR 中信息量最大的类型 —— 四种协议的多模态与工具调用表达差异极大，
/// 全部收敛到这个枚举。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// 纯文本。
    Text {
        /// 文本内容。
        text: String,
    },
    /// 图片。
    Image {
        /// 数据来源。
        source: MediaSource,
        /// MIME 类型。Anthropic 的 base64 图片**必须**带 media_type。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        /// OpenAI 的 `detail` 提示（`auto`/`low`/`high`）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// 音频。
    Audio {
        /// 数据来源。
        source: MediaSource,
        /// 音频格式（`wav`/`mp3`/...）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    /// 文件/文档（PDF 等）。
    File {
        /// 数据来源。
        source: MediaSource,
        /// MIME 类型。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        /// 文件名。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// 模型的推理过程（Anthropic `thinking` / Gemini `thought` /
    /// OpenAI `reasoning`）。
    Thinking {
        /// 推理文本。
        text: String,
        /// Anthropic 的推理签名。
        ///
        /// **必须保留**：多轮工具调用时若丢失 signature，Anthropic 上游会
        /// 拒绝整个请求。转到其他协议时它会进 [`Extensions`]。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// 被上游加密/脱敏的推理内容，只能原样回传。
    RedactedThinking {
        /// 不透明数据。
        data: String,
    },
    /// 模型请求调用工具。
    ToolUse {
        /// 调用 ID，工具结果靠它关联回来。
        id: String,
        /// 工具名。
        name: String,
        /// 入参。
        input: Value,
        /// Gemini 的 `thoughtSignature`。
        ///
        /// **必须保留**：Gemini 3 强制要求多轮对话回传的 `functionCall`
        /// 携带思维签名，缺失直接 400。与 [`ContentPart::Thinking::signature`]
        /// 是两个独立通道 —— Gemini 把签名挂在工具调用上而非思考块上。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// 工具执行结果。
    ToolResult {
        /// 对应的 [`ContentPart::ToolUse::id`]。
        id: String,
        /// 工具名。
        ///
        /// Gemini 的 `functionResponse.name` 必须是**函数声明名**而非调用
        /// ID —— OpenAI 系协议的工具结果只带 id，缺了这个字段就只能拿 id
        /// 冒充函数名，跨协议转到 Gemini 时配对失败。解码时尽力填充。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// 结果内容。用 `Vec<ContentPart>` 而非 `String`，因为 Anthropic
        /// 允许工具返回图片。
        content: Vec<ContentPart>,
        /// 是否为错误结果。
        #[serde(default)]
        is_error: bool,
    },
    /// 模型拒答（OpenAI `refusal`）。
    Refusal {
        /// 拒答说明。
        text: String,
    },
    /// 无法归一化的协议专属内容块，只在同协议直通时原样还原。
    ///
    /// 典型来源：Anthropic 的 `server_tool_use`/`web_search_tool_result`
    /// 等服务端工具块 —— 官方要求多轮对话原样回传，翻译成其他协议没有
    /// 意义，但直通场景丢弃它们会损坏对话历史。目标编码器的处理规则：
    /// 协议匹配 → 原样输出；不匹配 → 丢弃。
    Opaque {
        /// 来源协议（如 `"messages"`）。
        protocol: String,
        /// 原始 JSON 块。
        value: Value,
    },
}

impl ContentPart {
    /// 便捷构造文本片段。
    pub fn text(s: impl Into<String>) -> Self {
        ContentPart::Text { text: s.into() }
    }

    /// 若为文本片段则取出其内容。
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentPart::Text { text } => Some(text),
            _ => None,
        }
    }
}

/// 一条消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// 角色。
    pub role: Role,
    /// 内容片段。
    pub content: Vec<ContentPart>,
    /// 说话者名（OpenAI `name` 字段）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Message {
    /// 构造一条消息。
    pub fn new(role: Role, content: Vec<ContentPart>) -> Self {
        Self {
            role,
            content,
            name: None,
        }
    }

    /// 构造一条纯文本消息。
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self::new(role, vec![ContentPart::text(text)])
    }

    /// 把所有文本片段拼接起来。
    pub fn text_content(&self) -> String {
        let mut out = String::new();
        for part in &self.content {
            if let ContentPart::Text { text } = part {
                out.push_str(text);
            }
        }
        out
    }

    /// 该消息是否只含纯文本。
    ///
    /// 用于优化：纯文本消息在 OpenAI/Anthropic 里可以用字符串而非数组表达，
    /// 输出更紧凑也更兼容那些实现不完整的中转站。
    pub fn is_plain_text(&self) -> bool {
        !self.content.is_empty()
            && self
                .content
                .iter()
                .all(|p| matches!(p, ContentPart::Text { .. }))
    }
}

/// 工具（函数）声明。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    /// 工具名。
    pub name: String,
    /// 描述。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema 入参定义。
    pub parameters: Value,
    /// 是否启用严格模式（OpenAI `strict`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// 工具选择策略。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// 未指定，交给上游默认行为。
    #[default]
    Unspecified,
    /// 模型自行决定。
    Auto,
    /// 必须调用某个工具（任意一个）。
    Required,
    /// 禁止调用工具。
    None,
    /// 必须调用指定工具。
    Tool(String),
}

/// 采样参数。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Sampling {
    /// 温度。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// 核采样。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Top-K。仅 Anthropic 与 Gemini 支持。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// 频率惩罚。仅 OpenAI 支持。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    /// 存在惩罚。仅 OpenAI 支持。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// 停止序列。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// 随机种子。仅 OpenAI Chat 支持。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    /// 候选数量。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_count: Option<u32>,
}

/// 推理/思考配置。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReasoningConfig {
    /// 定性档位（OpenAI `reasoning_effort`：`minimal`/`low`/`medium`/`high`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// 定量预算（Anthropic `budget_tokens` / Gemini `thinkingBudget`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
    /// 是否把推理内容包含在输出里。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_thoughts: Option<bool>,
}

impl ReasoningConfig {
    /// 把定性档位折算成 token 预算。
    ///
    /// 从 OpenAI（只有档位）转到 Anthropic/Gemini（要预算）时必须做这个折算，
    /// 否则思考功能会被静默关闭。
    pub fn budget_or_from_effort(&self, max_output: Option<u32>) -> Option<u32> {
        if let Some(b) = self.budget_tokens {
            return Some(b);
        }
        let effort = self.effort.as_deref()?;
        let ceiling = max_output.unwrap_or(32_000);
        // Anthropic 要求 1024 <= budget_tokens < max_tokens：上限装不下最小
        // 预算时不存在合法取值，只能放弃思考，而不是发一个必然被拒的请求。
        if ceiling <= 1_024 {
            return None;
        }
        let budget = match effort {
            "minimal" | "none" => return None,
            "low" => ceiling / 5,
            "medium" => ceiling / 2,
            "high" => ceiling * 4 / 5,
            _ => return None,
        };
        Some(budget.max(1_024).min(ceiling - 1))
    }

    /// 把 token 预算折算成定性档位。
    pub fn effort_or_from_budget(&self, max_output: Option<u32>) -> Option<&'static str> {
        if let Some(e) = self.effort.as_deref() {
            return Some(match e {
                "minimal" | "none" => "minimal",
                "low" => "low",
                "high" => "high",
                _ => "medium",
            });
        }
        let budget = self.budget_tokens?;
        let ceiling = max_output.unwrap_or(32_000).max(1);
        let ratio = f64::from(budget) / f64::from(ceiling);
        Some(if ratio >= 0.65 {
            "high"
        } else if ratio >= 0.35 {
            "medium"
        } else {
            "low"
        })
    }
}

/// 响应格式约束。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// 自由文本。
    Text,
    /// 任意合法 JSON。
    JsonObject,
    /// 符合给定 JSON Schema。
    JsonSchema {
        /// Schema 名称。
        name: String,
        /// Schema 本体。
        schema: Value,
        /// 是否严格遵循。
        #[serde(default)]
        strict: bool,
    },
}

/// 统一请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnifiedRequest {
    /// 模型名（对外名，路由后会被替换为上游名）。
    pub model: String,
    /// 系统指令。
    ///
    /// 独立于 [`Self::messages`]，因为 Anthropic 与 Gemini 把 system 放在
    /// 顶层字段而非消息数组里。解码时统一提取到这里。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<ContentPart>,
    /// 对话消息。
    pub messages: Vec<Message>,
    /// 工具声明。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    /// 工具选择策略。
    #[serde(default)]
    pub tool_choice: ToolChoice,
    /// 是否允许并行工具调用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// 采样参数。
    #[serde(default)]
    pub sampling: Sampling,
    /// 最大输出 token 数。
    ///
    /// 归一化了 OpenAI `max_tokens`/`max_completion_tokens`、Anthropic
    /// `max_tokens`（必填）、Gemini `generationConfig.maxOutputTokens`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// 是否流式。
    #[serde(default)]
    pub stream: bool,
    /// 流式时是否要求上游回传 usage。
    #[serde(default)]
    pub stream_include_usage: bool,
    /// 推理配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    /// 响应格式约束。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    /// 终端用户标识。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// 协议专属字段。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: Extensions,
}

impl UnifiedRequest {
    /// 构造一个最小请求。
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            system: Vec::new(),
            messages,
            tools: Vec::new(),
            tool_choice: ToolChoice::Unspecified,
            parallel_tool_calls: None,
            sampling: Sampling::default(),
            max_output_tokens: None,
            stream: false,
            stream_include_usage: false,
            reasoning: None,
            response_format: None,
            user: None,
            extensions: Extensions::new(),
        }
    }

    /// 读取某个协议专属字段。
    pub fn extension(&self, key: &str) -> Option<&Value> {
        self.extensions.get(key)
    }

    /// 写入某个协议专属字段。
    pub fn set_extension(&mut self, key: impl Into<String>, value: Value) {
        self.extensions.insert(key.into(), value);
    }

    /// 系统指令的纯文本形式。
    pub fn system_text(&self) -> String {
        let mut out = String::new();
        for part in &self.system {
            if let ContentPart::Text { text } = part {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        out
    }
}

/// 停止原因。
#[derive(Debug, Clone, PartialEq, Eq, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// 自然结束。
    Stop,
    /// 达到 token 上限。
    MaxTokens,
    /// 命中停止序列。
    StopSequence,
    /// 模型要求调用工具。
    ToolUse,
    /// 内容被过滤/安全拦截。
    ContentFilter,
    /// 模型拒答。
    Refusal,
    /// 上游暂停回合（Anthropic `pause_turn`）。
    PauseTurn,
    /// 其他/未知。
    Other,
}

/// Token 用量。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// 输入 token。
    pub input_tokens: u64,
    /// 输出 token。
    pub output_tokens: u64,
    /// 缓存命中的输入 token。
    pub cached_input_tokens: u64,
    /// 写入缓存的 token（Anthropic `cache_creation_input_tokens`）。
    pub cache_write_tokens: u64,
    /// 推理消耗的 token。
    pub reasoning_tokens: u64,
}

impl Usage {
    /// 总 token 数。
    pub const fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// 是否全为零（上游没回 usage）。
    pub const fn is_empty(&self) -> bool {
        self.input_tokens == 0 && self.output_tokens == 0
    }

    /// 把各协议的 usage 统一到**计费口径**：`input_tokens` = 总输入
    /// （含缓存读与缓存写）。
    ///
    /// OpenAI 与 Gemini 的输入计数已包含缓存命中；Anthropic 的
    /// `input_tokens` 既不含 `cache_read_input_tokens` 也不含
    /// `cache_creation_input_tokens` —— 不归一直接记账，Claude 的缓存
    /// 流量就从账单里消失了。只用于记账/计价，协议往返仍用原始值。
    pub fn billing_normalized(mut self, upstream: refract_core::Protocol) -> Self {
        if upstream == refract_core::Protocol::Messages {
            self.input_tokens = self
                .input_tokens
                .saturating_add(self.cached_input_tokens)
                .saturating_add(self.cache_write_tokens);
        }
        self
    }

    /// 合并两个 Usage 快照（取各项最大值）。
    ///
    /// 大多数上游流式响应（如 OpenAI、Anthropic）在每个 SSE 块中发送的是全量累计用量，
    /// 取最大值可正确保留最终的完整账单数据。
    pub fn merge_max(&mut self, other: &Usage) {
        self.input_tokens = self.input_tokens.max(other.input_tokens);
        self.output_tokens = self.output_tokens.max(other.output_tokens);
        self.cached_input_tokens = self.cached_input_tokens.max(other.cached_input_tokens);
        self.cache_write_tokens = self.cache_write_tokens.max(other.cache_write_tokens);
        self.reasoning_tokens = self.reasoning_tokens.max(other.reasoning_tokens);
    }

    /// 累加两个 Usage（各项求和）。
    pub fn merge_sum(&mut self, other: &Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
    }
}

/// 统一响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnifiedResponse {
    /// 响应 ID。
    pub id: String,
    /// 实际使用的模型。
    pub model: String,
    /// 创建时间（Unix 秒）。
    pub created: i64,
    /// 输出内容。
    pub content: Vec<ContentPart>,
    /// 停止原因。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    /// 命中的停止序列。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    /// Token 用量。
    #[serde(default)]
    pub usage: Usage,
    /// 协议专属字段。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: Extensions,
}

impl UnifiedResponse {
    /// 构造一个响应。
    pub fn new(id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            model: model.into(),
            created: chrono::Utc::now().timestamp(),
            content: Vec::new(),
            stop_reason: None,
            stop_sequence: None,
            usage: Usage::default(),
            extensions: Extensions::new(),
        }
    }

    /// 输出内容中的纯文本部分。
    pub fn text(&self) -> String {
        let mut out = String::new();
        for part in &self.content {
            if let ContentPart::Text { text } = part {
                out.push_str(text);
            }
        }
        out
    }

    /// 输出内容中的工具调用。
    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.content.iter().filter_map(|p| match p {
            ContentPart::ToolUse {
                id, name, input, ..
            } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn data_uri_is_split_into_source_and_mime() {
        let (src, mime) = MediaSource::parse_data_uri("data:image/png;base64,iVBORw0KGgo=");
        assert_eq!(src, MediaSource::Base64("iVBORw0KGgo=".into()));
        assert_eq!(mime.as_deref(), Some("image/png"));
    }

    #[test]
    fn plain_url_stays_a_url() {
        let (src, mime) = MediaSource::parse_data_uri("https://example.com/cat.png");
        assert_eq!(src, MediaSource::Url("https://example.com/cat.png".into()));
        assert_eq!(mime, None);
    }

    #[test]
    fn non_base64_data_uri_is_not_treated_as_base64() {
        let (src, _) = MediaSource::parse_data_uri("data:text/plain,hello");
        assert!(matches!(src, MediaSource::Url(_)));
    }

    #[test]
    fn base64_source_renders_back_to_data_uri() {
        let src = MediaSource::Base64("QUJD".into());
        assert_eq!(
            src.to_data_uri(Some("image/jpeg")),
            "data:image/jpeg;base64,QUJD"
        );
        // 缺 mime 时给一个不会让上游炸掉的兜底值。
        assert_eq!(
            src.to_data_uri(None),
            "data:application/octet-stream;base64,QUJD"
        );
    }

    #[test]
    fn message_text_content_concatenates_text_parts_only() {
        let msg = Message::new(
            Role::User,
            vec![
                ContentPart::text("hello "),
                ContentPart::Image {
                    source: MediaSource::Url("https://x/y.png".into()),
                    mime: None,
                    detail: None,
                },
                ContentPart::text("world"),
            ],
        );
        assert_eq!(msg.text_content(), "hello world");
        assert!(!msg.is_plain_text());
    }

    #[test]
    fn plain_text_detection_requires_nonempty_content() {
        assert!(Message::text(Role::User, "hi").is_plain_text());
        assert!(!Message::new(Role::User, vec![]).is_plain_text());
    }

    #[test]
    fn system_text_joins_parts_with_newlines() {
        let mut req = UnifiedRequest::new("m", vec![]);
        req.system = vec![
            ContentPart::text("be terse"),
            ContentPart::text("be correct"),
        ];
        assert_eq!(req.system_text(), "be terse\nbe correct");
    }

    #[test]
    fn effort_converts_to_budget_with_anthropic_minimum() {
        let cfg = ReasoningConfig {
            effort: Some("low".into()),
            ..Default::default()
        };
        // 4096/5 = 819，低于 Anthropic 的 1024 下限，应被抬到 1024。
        assert_eq!(cfg.budget_or_from_effort(Some(4_096)), Some(1_024));

        let high = ReasoningConfig {
            effort: Some("high".into()),
            ..Default::default()
        };
        assert_eq!(high.budget_or_from_effort(Some(10_000)), Some(8_000));
    }

    #[test]
    fn minimal_effort_means_no_thinking_budget() {
        let cfg = ReasoningConfig {
            effort: Some("minimal".into()),
            ..Default::default()
        };
        assert_eq!(cfg.budget_or_from_effort(Some(8_000)), None);
    }

    #[test]
    fn effort_budget_respects_max_output_ceiling() {
        let high = ReasoningConfig {
            effort: Some("high".into()),
            ..Default::default()
        };
        // Anthropic 要求 budget < max_tokens：上限勉强容纳最小预算时取 1024。
        assert_eq!(high.budget_or_from_effort(Some(1_200)), Some(1_024));
        // 上限装不下 1024 时不存在合法预算，只能放弃思考。
        assert_eq!(high.budget_or_from_effort(Some(1_024)), None);
        assert_eq!(high.budget_or_from_effort(Some(512)), None);
    }

    #[test]
    fn explicit_budget_wins_over_effort() {
        let cfg = ReasoningConfig {
            effort: Some("low".into()),
            budget_tokens: Some(9_999),
            ..Default::default()
        };
        assert_eq!(cfg.budget_or_from_effort(Some(4_096)), Some(9_999));
    }

    #[test]
    fn budget_converts_back_to_effort_tier() {
        let low = ReasoningConfig {
            budget_tokens: Some(1_000),
            ..Default::default()
        };
        assert_eq!(low.effort_or_from_budget(Some(10_000)), Some("low"));

        let mid = ReasoningConfig {
            budget_tokens: Some(5_000),
            ..Default::default()
        };
        assert_eq!(mid.effort_or_from_budget(Some(10_000)), Some("medium"));

        let high = ReasoningConfig {
            budget_tokens: Some(8_000),
            ..Default::default()
        };
        assert_eq!(high.effort_or_from_budget(Some(10_000)), Some("high"));
    }

    #[test]
    fn usage_totals_input_and_output() {
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        };
        assert_eq!(usage.total(), 15);
        assert!(!usage.is_empty());
        assert!(Usage::default().is_empty());
    }

    #[test]
    fn response_extracts_text_and_tool_uses() {
        let mut resp = UnifiedResponse::new("resp_1", "gpt-4o");
        resp.content = vec![
            ContentPart::text("calling tool"),
            ContentPart::ToolUse {
                signature: None,
                id: "call_1".into(),
                name: "get_weather".into(),
                input: serde_json::json!({"city": "Tokyo"}),
            },
        ];
        assert_eq!(resp.text(), "calling tool");
        let uses: Vec<_> = resp.tool_uses().collect();
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].0, "call_1");
        assert_eq!(uses[0].1, "get_weather");
    }

    #[test]
    fn thinking_signature_survives_ir_roundtrip() {
        // 这是硬性要求：Anthropic 多轮工具调用丢了 signature 会被上游拒绝。
        let part = ContentPart::Thinking {
            text: "let me think".into(),
            signature: Some("sig_abc123".into()),
        };
        let json = serde_json::to_string(&part).unwrap();
        let back: ContentPart = serde_json::from_str(&json).unwrap();
        assert_eq!(part, back);
    }

    #[test]
    fn extensions_are_namespaced_per_protocol() {
        let mut req = UnifiedRequest::new("m", vec![]);
        req.set_extension(
            "gemini.safetySettings",
            serde_json::json!([{"category": "X"}]),
        );
        req.set_extension(
            "responses.previous_response_id",
            serde_json::json!("resp_9"),
        );
        assert!(req.extension("gemini.safetySettings").is_some());
        assert!(req.extension("responses.previous_response_id").is_some());
        assert!(req.extension("chat.seed").is_none());
    }

    #[test]
    fn tool_result_can_carry_images() {
        // Anthropic 允许工具返回图片，IR 必须能表达。
        let part = ContentPart::ToolResult {
            name: None,
            id: "call_1".into(),
            content: vec![ContentPart::Image {
                source: MediaSource::Base64("AAA".into()),
                mime: Some("image/png".into()),
                detail: None,
            }],
            is_error: false,
        };
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json["type"], "tool_result");
        let back: ContentPart = serde_json::from_value(json).unwrap();
        assert_eq!(part, back);
    }
}
