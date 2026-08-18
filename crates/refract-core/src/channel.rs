//! 渠道模型。
//!
//! 核心设计：**渠道类型即协议**，厂商无关。单协议渠道与聚合渠道在运行时统一
//! 表达为「渠道 + 若干协议端点」，路由层只认 `(Channel, ChannelEndpoint)` 二元组，
//! 不需要为两种渠道形态写分支。

use serde::{Deserialize, Serialize};

use crate::address::UpstreamAddress;
use crate::protocol::{Protocol, ProtocolSet};

/// 渠道 ID。
pub type ChannelId = i64;

/// 渠道类型。
///
/// 对应需求 1：只有 chat / res / message / gemini / 聚合 五种。
///
/// 序列化为**扁平字符串**：`"chat"` / `"responses"` / `"messages"` / `"gemini"` /
/// `"aggregate"`。手写 serde 而不用 derive，是因为 `#[serde(untagged)]` 要求
/// untagged 变体排在枚举末尾，而把 `Aggregate` 排在后面会让 `native_protocol`
/// 的匹配顺序读起来别扭；手写实现让 wire format 与内存布局解耦。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    /// 单协议渠道，其原生协议为内层值。
    Single(Protocol),
    /// 聚合渠道：一个渠道内挂载多个协议端点。
    Aggregate,
}

/// 聚合渠道在 wire format 中的标识。
const AGGREGATE_TAG: &str = "aggregate";

impl Serialize for ChannelKind {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ChannelKind {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let raw = <std::borrow::Cow<'de, str>>::deserialize(de)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

impl std::str::FromStr for ChannelKind {
    type Err = ParseChannelKindError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == AGGREGATE_TAG {
            return Ok(ChannelKind::Aggregate);
        }
        s.parse::<Protocol>()
            .map(ChannelKind::Single)
            .map_err(|_| ParseChannelKindError(s.to_owned()))
    }
}

/// 解析 [`ChannelKind`] 失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown channel kind: {0}")]
pub struct ParseChannelKindError(pub String);

impl ChannelKind {
    /// 稳定字符串标识。
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Single(p) => p.as_str(),
            ChannelKind::Aggregate => "aggregate",
        }
    }

    /// 单协议渠道的原生协议。
    pub const fn native_protocol(self) -> Option<Protocol> {
        match self {
            ChannelKind::Single(p) => Some(p),
            ChannelKind::Aggregate => None,
        }
    }

    /// 是否为聚合渠道。
    pub const fn is_aggregate(self) -> bool {
        matches!(self, ChannelKind::Aggregate)
    }
}

/// 协议转换策略。
///
/// 对应需求 4：转换开关 + 可转换协议勾选集。未勾选的协议打进来直接报错。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranscodePolicy {
    /// 协议转换总开关，默认关闭。
    pub enabled: bool,
    /// 允许从哪些入口协议转换过来。
    pub accepted: ProtocolSet,
}

impl TranscodePolicy {
    /// 关闭转换：只接受原生协议。
    pub const DISABLED: Self = Self {
        enabled: false,
        accepted: ProtocolSet::EMPTY,
    };

    /// 判断一个入口协议能否被该端点服务。
    ///
    /// 原生协议永远放行；非原生协议必须同时满足「开关打开」与「已勾选」。
    pub fn can_serve(&self, inbound: Protocol, native: Protocol) -> bool {
        inbound == native || (self.enabled && self.accepted.contains(inbound))
    }

    /// 该策略实际能服务的全部入口协议。
    pub fn served_protocols(&self, native: Protocol) -> ProtocolSet {
        let mut set = if self.enabled {
            self.accepted
        } else {
            ProtocolSet::EMPTY
        };
        set.insert(native);
        set
    }
}

/// 上游凭据。
///
/// 用 newtype 包起来是为了让 [`std::fmt::Debug`] 不泄漏密钥 —— 日志里出现明文
/// API key 是真实事故。
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Credential(String);

impl Credential {
    /// 由明文构造。
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// 明文密钥。仅应在构造上游请求头时调用。
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }

    /// 供 UI 展示的脱敏形式，如 `sk-a…9f2c`。
    pub fn masked(&self) -> String {
        let s = self.0.trim();
        let chars: Vec<char> = s.chars().collect();
        match chars.len() {
            0 => String::new(),
            1..=8 => "•".repeat(chars.len()),
            n => {
                let head: String = chars[..4].iter().collect();
                let tail: String = chars[n - 4..].iter().collect();
                format!("{head}…{tail}")
            }
        }
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Credential").field(&self.masked()).finish()
    }
}

/// 多密钥的使用策略。
///
/// 渠道可以配置多把上游密钥（`credentials` 池）。端点级 `credential` 覆盖始终优先；
/// 没有覆盖时才轮到池子按本策略挑钥匙。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyStrategy {
    /// 黏性：同一调用方（网关 API 密钥）固定使用池中的一把钥匙，
    /// 直到该钥匙出错或渠道变化。无法识别调用方时退化为轮询。
    Sticky,
    /// 轮询：按渠道记忆游标依次取用，摊平速率配额。
    #[default]
    RoundRobin,
    /// 随机：每次请求独立随机抽取。
    Random,
}

impl KeyStrategy {
    /// 数据库与 API 中的字符串形式。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sticky => "sticky",
            Self::RoundRobin => "round_robin",
            Self::Random => "random",
        }
    }

    /// 从字符串解析；未知值回落到默认策略而不是拒绝整条配置。
    pub fn parse(value: &str) -> Self {
        match value {
            "sticky" => Self::Sticky,
            "random" => Self::Random,
            _ => Self::default(),
        }
    }
}

impl std::fmt::Display for KeyStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 端点一次请求的凭据来源：单钥或多钥池。
///
/// 由 [`Channel::key_pool`] 按「端点覆盖 > 顶层默认 + 池 > 纯池」解析而来。
/// 路由执行器对 `Single` 直接取用；对 `Pool` 按 [`KeyStrategy`] 挑钥匙，
/// 并在单钥报鉴权类错误时轮换下一把。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPool<'a> {
    /// 单钥：端点级覆盖，或渠道只配了一把钥匙。
    Single(&'a Credential),
    /// 多钥池。`first` 是顶层默认钥匙，`rest` 是额外钥匙；逻辑顺序为
    /// `[first, rest…]`。
    Pool {
        /// 顶层默认钥匙。
        first: &'a Credential,
        /// 额外的池钥匙。
        rest: &'a [Credential],
        /// 使用策略。
        strategy: KeyStrategy,
    },
}

impl<'a> KeyPool<'a> {
    /// 池内钥匙数量，至少为 1。
    pub fn len(&self) -> usize {
        match self {
            Self::Single(_) => 1,
            Self::Pool { rest, .. } => rest.len().saturating_add(1),
        }
    }

    /// 恒为 `false`：池在构造上至少有一把钥匙，这是与 `len` 配对的不变量。
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// 是否单钥（无需轮换）。
    pub fn is_single(&self) -> bool {
        matches!(self, Self::Single(_))
    }

    /// 池的使用策略；单钥恒为轮询（只有一把，策略无意义）。
    pub fn strategy(&self) -> KeyStrategy {
        match self {
            Self::Single(_) => KeyStrategy::default(),
            Self::Pool { strategy, .. } => *strategy,
        }
    }

    /// 取第 `index` 把钥匙，越界自动取模。
    pub fn key_at(&self, index: usize) -> &'a Credential {
        match self {
            Self::Single(key) => key,
            Self::Pool { first, rest, .. } => {
                if rest.is_empty() {
                    return first;
                }
                let index = index % rest.len().saturating_add(1);
                if index == 0 { first } else { &rest[index - 1] }
            }
        }
    }
}

/// 一条模型条目。
///
/// `alias` 为空表示对外名与上游名相同；非空则对外暴露 `alias`，打上游时用 `upstream`。
/// 这比 new-api 的 `model_mapping` JSON 字符串更直接：映射关系和模型列表是同一份数据，
/// 不会出现「列表里有但映射里没有」的不一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// 对外暴露的模型名。
    pub name: String,
    /// 打到上游时使用的模型名。为空则同 `name`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
}

impl ModelEntry {
    /// 对外名与上游名一致的条目。
    pub fn plain(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            upstream: None,
        }
    }

    /// 带重映射的条目。
    pub fn mapped(name: impl Into<String>, upstream: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            upstream: Some(upstream.into()),
        }
    }

    /// 打到上游时应使用的模型名。
    pub fn upstream_name(&self) -> &str {
        self.upstream.as_deref().unwrap_or(&self.name)
    }
}

/// 渠道的一个协议端点。
///
/// 单协议渠道恰好有一个；聚合渠道有 1~4 个（需求 3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelEndpoint {
    /// 该端点的原生协议。
    pub protocol: Protocol,
    /// 端点优先顺序，数值越小越优先（需求 5）。
    #[serde(default)]
    pub order: u16,
    /// 是否启用。
    #[serde(default = "crate::default_true")]
    pub enabled: bool,
    /// 端点地址。未自定义时继承渠道默认地址。
    #[serde(default)]
    pub address: UpstreamAddress,
    /// 端点凭据。为 `None` 时继承渠道默认凭据。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<Credential>,
    /// 该端点提供的模型。
    #[serde(default)]
    pub models: Vec<ModelEntry>,
    /// 该端点的协议转换策略。
    #[serde(default)]
    pub transcode: TranscodePolicy,
}

impl ChannelEndpoint {
    /// 构造一个仅指定协议的端点，其余取默认值。
    pub fn new(protocol: Protocol) -> Self {
        Self {
            protocol,
            order: 0,
            enabled: true,
            address: UpstreamAddress::default(),
            credential: None,
            models: Vec::new(),
            transcode: TranscodePolicy::DISABLED,
        }
    }

    /// 查找对外模型名对应的条目。
    pub fn find_model(&self, name: &str) -> Option<&ModelEntry> {
        self.models.iter().find(|m| m.name == name)
    }
}

/// 上游 HTTP 200 响应处理策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmptyResponseRetryPolicy {
    /// 从首字节到响应完成的最长判定窗口（秒）。0 = 关闭。
    pub window_secs: u32,
    /// 同一渠道最多额外重试次数。0 = 关闭。
    pub max_retries: u32,
    /// 是否把不符合所配置协议的 HTTP 200 响应转换为明确的 500 错误。
    pub reject_nonstandard_200: bool,
}

impl Default for EmptyResponseRetryPolicy {
    fn default() -> Self {
        Self {
            window_secs: 3,
            max_retries: 5,
            reject_nonstandard_200: false,
        }
    }
}

impl EmptyResponseRetryPolicy {
    /// 防止误配置制造超长缓冲或失控的上游请求循环。
    pub fn validate(self) -> Result<(), &'static str> {
        if self.window_secs > 3600 {
            return Err("empty response retry window must be at most 3600 seconds");
        }
        if self.max_retries > 100 {
            return Err("empty response retries must be at most 100");
        }
        Ok(())
    }

    /// 两个值任一为 0 都表示关闭。
    pub const fn enabled(self) -> bool {
        self.window_secs > 0 && self.max_retries > 0
    }
}

/// 渠道对全局空回复重试策略的逐项覆盖。`None` 表示继承全局值。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmptyResponseRetryOverride {
    /// 判定窗口覆盖值。
    pub window_secs: Option<u32>,
    /// 最大重试次数覆盖值。
    pub max_retries: Option<u32>,
}

impl EmptyResponseRetryOverride {
    /// 是否完全继承全局设置。
    pub const fn is_inherited(&self) -> bool {
        self.window_secs.is_none() && self.max_retries.is_none()
    }

    /// 把渠道覆盖应用到全局策略。
    pub fn resolve(self, global: EmptyResponseRetryPolicy) -> EmptyResponseRetryPolicy {
        EmptyResponseRetryPolicy {
            window_secs: self.window_secs.unwrap_or(global.window_secs),
            max_retries: self.max_retries.unwrap_or(global.max_retries),
            reject_nonstandard_200: global.reject_nonstandard_200,
        }
    }

    /// 校验已填写的覆盖值。
    pub fn validate(self) -> Result<(), &'static str> {
        self.resolve(EmptyResponseRetryPolicy {
            window_secs: 0,
            max_retries: 0,
            reject_nonstandard_200: false,
        })
        .validate()
    }
}

/// 上游渠道。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    /// 主键。新建时为 0。
    #[serde(default)]
    pub id: ChannelId,
    /// 所属者。当前恒为 1，为将来的多用户预留。
    #[serde(default = "crate::default_owner")]
    pub owner_id: i64,
    /// 展示名。
    pub name: String,
    /// 渠道类型。
    pub kind: ChannelKind,
    /// 是否启用。
    #[serde(default = "crate::default_true")]
    pub enabled: bool,
    /// 优先级，数值越大越优先。
    #[serde(default)]
    pub priority: i32,
    /// 同优先级内的加权随机权重。
    #[serde(default = "crate::default_weight")]
    pub weight: u32,
    /// 渠道默认凭据，端点未单独配置时使用。
    #[serde(default)]
    pub credential: Credential,
    /// 多密钥池：一行一把的额外上游密钥。空时回落到 `credential` 单钥语义。
    ///
    /// 端点级 `credential` 覆盖优先于池子；池子又优先于顶层 `credential`。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<Credential>,
    /// 多密钥池的使用策略。
    #[serde(default)]
    pub key_strategy: KeyStrategy,
    /// 渠道默认地址，端点未单独配置时使用。
    #[serde(default)]
    pub address: UpstreamAddress,
    /// 协议端点。
    pub endpoints: Vec<ChannelEndpoint>,
    /// 自由标签，用于分组筛选。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// 请求超时（秒）。0 表示用全局默认。
    #[serde(default)]
    pub timeout_secs: u32,
    /// 出站代理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// 注入到上游请求体的参数覆盖，必须是 JSON 对象。
    ///
    /// 顶层键直接合并进所有端点的请求体；键名恰好是协议名（`chat` /
    /// `responses` / `messages` / `gemini`）且值为对象时，视为该协议专属的
    /// 覆盖组，只在打到对应协议端点时展开 —— 聚合渠道用它避免把 Chat 的
    /// 顶层采样参数盲注进 Gemini（Gemini 的采样参数在 `generationConfig`
    /// 里，顶层未知字段会被 400 拒绝）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param_override: Option<serde_json::Value>,
    /// 备注。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// 是否因终态错误（凭据失效等）被网关自动禁用。
    ///
    /// 与手动禁用分开：自动禁用的渠道参与定时重测自愈，手动禁用的不碰。
    /// 手动重新启用会清掉这个标记。
    #[serde(default)]
    pub auto_disabled: bool,
    /// 上游余额缓存（美元或中转站自定币种）。观测数据，非配置 ——
    /// 由余额刷新单独更新，渠道编辑不触碰。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<f64>,
    /// 余额最后刷新时间。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance_updated_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 注入到上游请求的自定义头。`param_override` 只管 body ——
    /// 要求自有鉴权头或机房路由头的中转站靠这个。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_headers: Vec<(String, String)>,
    /// 连通性测试与定时重测使用的模型；空则用被测端点的第一个模型。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_model: Option<String>,
    /// HTTP 200 空回复重试覆盖。两项都为空时完全继承全局设置。
    #[serde(default)]
    pub empty_response_retry: EmptyResponseRetryOverride,
}

/// 渠道校验失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChannelError {
    /// 渠道名为空。
    #[error("channel name must not be empty")]
    EmptyName,
    /// 端点列表为空。
    #[error("channel must have at least one protocol endpoint")]
    NoEndpoints,
    /// 单协议渠道的端点数不为 1。
    #[error("single-protocol channel must have exactly one endpoint, got {0}")]
    SingleMustHaveOneEndpoint(usize),
    /// 单协议渠道的端点协议与渠道类型不符。
    #[error("single-protocol channel of kind `{expected}` cannot host a `{actual}` endpoint")]
    EndpointProtocolMismatch {
        /// 渠道声明的协议。
        expected: Protocol,
        /// 端点实际协议。
        actual: Protocol,
    },
    /// 同一协议出现多个端点。
    #[error("duplicate endpoint for protocol `{0}`")]
    DuplicateProtocol(Protocol),
    /// 某个端点既没有自己的凭据，渠道默认凭据也为空。
    #[error("endpoint `{0}` has no credential and the channel default is empty")]
    MissingCredential(Protocol),
    /// 多密钥池里存在空行 —— 一行一把，空行只会制造故障。
    #[error("multi-key pool contains an empty credential; remove the blank line")]
    EmptyPoolCredential,
    /// 转换策略把原生协议自身也勾上了 —— 无意义，且暗示配置理解有误。
    #[error("endpoint `{0}` lists its own native protocol as a transcode target")]
    SelfTranscode(Protocol),
    /// 参数覆盖不是 JSON 对象 —— 执行器只会合并对象，其他形状会被静默忽略，
    /// 与其让用户困惑「为什么不生效」，不如在保存时就拒绝。
    #[error("param_override must be a JSON object, got {0}")]
    ParamOverrideNotObject(&'static str),
    /// 自定义头名不是合法的 HTTP header 名。
    #[error("extra header name `{0}` is not a valid HTTP header name")]
    InvalidExtraHeader(String),
    /// 自定义头试图覆盖网关掌管的鉴权/传输语义头。
    #[error("extra header `{0}` is managed by the gateway and cannot be overridden")]
    ForbiddenExtraHeader(String),
    /// 自定义头值含 CR/LF —— 会导致响应拆分/头注入，必须在保存前拒绝。
    #[error("extra header `{0}` value must not contain CR or LF")]
    InvalidExtraHeaderValue(String),
    /// 端点模型条目的对外名为空。
    #[error("endpoint `{0}` has a model entry with an empty name")]
    EmptyModelName(Protocol),
    /// 模型条目映射到的上游名为空（`Some("")` 与缺失同名都无效）。
    #[error("model `{name}` maps to an empty upstream name")]
    EmptyUpstreamMapping {
        /// 对外模型名。
        name: String,
    },
    /// 地址开了非官方模式却没给 base_url，resolve 必然失败。
    #[error("endpoint `{0}` enables unofficial address but has no base_url")]
    MissingUnofficialBaseUrl(Protocol),
    /// 渠道默认地址开了非官方模式却没给 base_url。
    #[error("channel default address enables unofficial mode but has no base_url")]
    MissingDefaultUnofficialBaseUrl,
    /// 渠道空回复重试覆盖超出安全范围。
    #[error("{0}")]
    InvalidEmptyResponseRetry(&'static str),
}

impl Channel {
    /// 校验渠道配置的自洽性。
    ///
    /// 这些不变量是路由与转换层的前提，必须在写入存储前守住。
    pub fn validate(&self) -> Result<(), ChannelError> {
        if self.name.trim().is_empty() {
            return Err(ChannelError::EmptyName);
        }
        if self.endpoints.is_empty() {
            return Err(ChannelError::NoEndpoints);
        }
        if self.credentials.iter().any(|c| c.is_empty()) {
            return Err(ChannelError::EmptyPoolCredential);
        }

        if let Some(native) = self.kind.native_protocol() {
            if self.endpoints.len() != 1 {
                return Err(ChannelError::SingleMustHaveOneEndpoint(
                    self.endpoints.len(),
                ));
            }
            let actual = self.endpoints[0].protocol;
            if actual != native {
                return Err(ChannelError::EndpointProtocolMismatch {
                    expected: native,
                    actual,
                });
            }
        }

        let mut seen = ProtocolSet::EMPTY;
        for ep in &self.endpoints {
            if seen.contains(ep.protocol) {
                return Err(ChannelError::DuplicateProtocol(ep.protocol));
            }
            seen.insert(ep.protocol);

            if ep.transcode.accepted.contains(ep.protocol) {
                return Err(ChannelError::SelfTranscode(ep.protocol));
            }

            let has_own = ep.credential.as_ref().is_some_and(|c| !c.is_empty());
            if !has_own && self.credential.is_empty() && self.credentials.is_empty() {
                return Err(ChannelError::MissingCredential(ep.protocol));
            }

            for entry in &ep.models {
                if entry.name.trim().is_empty() {
                    return Err(ChannelError::EmptyModelName(ep.protocol));
                }
                if entry
                    .upstream
                    .as_deref()
                    .is_some_and(|u| u.trim().is_empty())
                {
                    return Err(ChannelError::EmptyUpstreamMapping {
                        name: entry.name.clone(),
                    });
                }
            }

            // 端点自定义了非官方地址就必须带 base_url，否则 resolve 必败。
            // 继承渠道默认的端点由下面的渠道级检查兜住。
            if ep.address.unofficial && base_url_missing(&ep.address) {
                return Err(ChannelError::MissingUnofficialBaseUrl(ep.protocol));
            }
        }

        if self.address.unofficial && base_url_missing(&self.address) {
            return Err(ChannelError::MissingDefaultUnofficialBaseUrl);
        }

        if let Some(value) = &self.param_override
            && !value.is_object()
        {
            let kind = match value {
                serde_json::Value::Null => "null",
                serde_json::Value::Bool(_) => "a boolean",
                serde_json::Value::Number(_) => "a number",
                serde_json::Value::String(_) => "a string",
                serde_json::Value::Array(_) => "an array",
                serde_json::Value::Object(_) => unreachable!(),
            };
            return Err(ChannelError::ParamOverrideNotObject(kind));
        }

        for (name, value) in &self.extra_headers {
            let normalized = name.trim().to_ascii_lowercase();
            if normalized.is_empty() || !normalized.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(ChannelError::InvalidExtraHeader(name.clone()));
            }
            // 鉴权与传输语义头由网关掌管 —— 允许覆盖等于允许配置出一个
            // 无法排障的「幽灵鉴权」。
            if matches!(
                normalized.as_str(),
                "authorization" | "host" | "content-length" | "content-type" | "x-api-key"
            ) {
                return Err(ChannelError::ForbiddenExtraHeader(normalized));
            }
            // 值里的 CR/LF 会演变成头注入/响应拆分，一律拒绝。
            if value.contains(['\r', '\n']) {
                return Err(ChannelError::InvalidExtraHeaderValue(name.clone()));
            }
        }

        self.empty_response_retry
            .validate()
            .map_err(ChannelError::InvalidEmptyResponseRetry)?;

        Ok(())
    }

    /// 端点实际生效的地址：自身未自定义则继承渠道默认。
    pub fn effective_address<'a>(&'a self, ep: &'a ChannelEndpoint) -> &'a UpstreamAddress {
        if ep.address.is_inherited() {
            &self.address
        } else {
            &ep.address
        }
    }

    /// 端点实际生效的凭据：端点覆盖 > 多密钥池首钥 > 顶层默认。
    ///
    /// 探测、余额查询、连通性测试等「只打一发」的场景统一用这个入口，
    /// 它们不需要也不应该消费轮询游标。
    pub fn effective_credential<'a>(&'a self, ep: &'a ChannelEndpoint) -> &'a Credential {
        self.key_pool(ep).key_at(0)
    }

    /// 端点的凭据来源：单钥或多钥池。
    ///
    /// 优先级：端点级覆盖 > 顶层默认 + `credentials` 池 > 纯池子。
    /// 池内密钥在 `validate()` 与存储加载时都保证非空。
    pub fn key_pool<'a>(&'a self, ep: &'a ChannelEndpoint) -> KeyPool<'a> {
        if let Some(own) = ep.credential.as_ref().filter(|c| !c.is_empty()) {
            return KeyPool::Single(own);
        }
        if !self.credential.is_empty() {
            if self.credentials.is_empty() {
                return KeyPool::Single(&self.credential);
            }
            return KeyPool::Pool {
                first: &self.credential,
                rest: &self.credentials,
                strategy: self.key_strategy,
            };
        }
        match self.credentials.split_first() {
            Some((first, rest)) => KeyPool::Pool {
                first,
                rest,
                strategy: self.key_strategy,
            },
            None => KeyPool::Single(&self.credential),
        }
    }

    /// 按 [`ChannelEndpoint::order`] 升序排列的端点引用。
    ///
    /// 需求 5：一个渠道内多个端点提供同一模型时，命中 order 最小者。
    pub fn endpoints_by_order(&self) -> Vec<&ChannelEndpoint> {
        let mut refs: Vec<&ChannelEndpoint> = self.endpoints.iter().filter(|e| e.enabled).collect();
        refs.sort_by_key(|e| (e.order, e.protocol));
        refs
    }

    /// 该渠道对外暴露的全部模型名（去重，保持首次出现顺序）。
    pub fn exposed_models(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for ep in self.endpoints_by_order() {
            for m in &ep.models {
                if !out.contains(&m.name.as_str()) {
                    out.push(&m.name);
                }
            }
        }
        out
    }
}

/// 非官方地址缺可用 base_url：未填或只有空白。
fn base_url_missing(address: &UpstreamAddress) -> bool {
    address
        .base_url
        .as_deref()
        .map(str::trim)
        .is_none_or(str::is_empty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn empty_response_retry_defaults_and_channel_overrides_resolve() {
        let global = EmptyResponseRetryPolicy::default();
        assert_eq!(global.window_secs, 3);
        assert_eq!(global.max_retries, 5);

        let inherited = EmptyResponseRetryOverride::default().resolve(global);
        assert_eq!(inherited, global);

        let partial = EmptyResponseRetryOverride {
            window_secs: Some(8),
            max_retries: None,
        }
        .resolve(global);
        assert_eq!(partial.window_secs, 8);
        assert_eq!(partial.max_retries, 5);
    }

    fn single(protocol: Protocol) -> Channel {
        Channel {
            id: 1,
            owner_id: 1,
            name: "test".into(),
            kind: ChannelKind::Single(protocol),
            enabled: true,
            priority: 0,
            weight: 1,
            credential: Credential::new("sk-test-key"),
            credentials: Vec::new(),
            key_strategy: KeyStrategy::default(),
            address: UpstreamAddress::default(),
            endpoints: vec![ChannelEndpoint {
                models: vec![ModelEntry::plain("gpt-4o")],
                ..ChannelEndpoint::new(protocol)
            }],
            tags: vec![],
            timeout_secs: 0,
            proxy: None,
            param_override: None,
            note: None,
            auto_disabled: false,
            balance: None,
            balance_updated_at: None,
            extra_headers: Vec::new(),
            test_model: None,
            empty_response_retry: EmptyResponseRetryOverride::default(),
        }
    }

    #[test]
    fn transcode_always_allows_native_protocol() {
        let policy = TranscodePolicy::DISABLED;
        assert!(policy.can_serve(Protocol::Chat, Protocol::Chat));
        assert!(!policy.can_serve(Protocol::Messages, Protocol::Chat));
    }

    #[test]
    fn transcode_requires_both_switch_and_checkbox() {
        // 勾了但没开总开关 → 拒绝。
        let off = TranscodePolicy {
            enabled: false,
            accepted: ProtocolSet::from_iter_protocols([Protocol::Messages]),
        };
        assert!(!off.can_serve(Protocol::Messages, Protocol::Chat));

        // 开了但没勾 → 拒绝。
        let unchecked = TranscodePolicy {
            enabled: true,
            accepted: ProtocolSet::from_iter_protocols([Protocol::Gemini]),
        };
        assert!(!unchecked.can_serve(Protocol::Messages, Protocol::Chat));

        // 都满足 → 放行。
        let on = TranscodePolicy {
            enabled: true,
            accepted: ProtocolSet::from_iter_protocols([Protocol::Messages]),
        };
        assert!(on.can_serve(Protocol::Messages, Protocol::Chat));
    }

    #[test]
    fn served_protocols_always_includes_native() {
        let policy = TranscodePolicy {
            enabled: true,
            accepted: ProtocolSet::from_iter_protocols([Protocol::Gemini]),
        };
        let served = policy.served_protocols(Protocol::Chat);
        assert!(served.contains(Protocol::Chat));
        assert!(served.contains(Protocol::Gemini));
        assert_eq!(served.len(), 2);
    }

    #[test]
    fn credential_debug_never_leaks_plaintext() {
        let cred = Credential::new("sk-proj-supersecretvalue");
        let rendered = format!("{cred:?}");
        assert!(!rendered.contains("supersecret"), "{rendered}");
        assert!(rendered.contains('…'), "{rendered}");
    }

    #[test]
    fn credential_masking_handles_short_values() {
        assert_eq!(Credential::new("").masked(), "");
        assert_eq!(Credential::new("abc").masked(), "•••");
        assert_eq!(Credential::new("sk-abcdefgh").masked(), "sk-a…efgh");
    }

    #[test]
    fn model_entry_maps_upstream_name() {
        assert_eq!(ModelEntry::plain("gpt-4o").upstream_name(), "gpt-4o");
        assert_eq!(
            ModelEntry::mapped("my-gpt", "gpt-4o-2026-01-01").upstream_name(),
            "gpt-4o-2026-01-01"
        );
    }

    #[test]
    fn valid_single_channel_passes() {
        assert_eq!(single(Protocol::Chat).validate(), Ok(()));
    }

    #[test]
    fn single_channel_rejects_extra_endpoints() {
        let mut ch = single(Protocol::Chat);
        ch.endpoints.push(ChannelEndpoint::new(Protocol::Messages));
        assert_eq!(
            ch.validate(),
            Err(ChannelError::SingleMustHaveOneEndpoint(2))
        );
    }

    #[test]
    fn single_channel_rejects_mismatched_endpoint_protocol() {
        let mut ch = single(Protocol::Chat);
        ch.endpoints[0].protocol = Protocol::Gemini;
        assert_eq!(
            ch.validate(),
            Err(ChannelError::EndpointProtocolMismatch {
                expected: Protocol::Chat,
                actual: Protocol::Gemini,
            })
        );
    }

    #[test]
    fn aggregate_rejects_duplicate_protocol() {
        let mut ch = single(Protocol::Chat);
        ch.kind = ChannelKind::Aggregate;
        ch.endpoints.push(ChannelEndpoint::new(Protocol::Chat));
        assert_eq!(
            ch.validate(),
            Err(ChannelError::DuplicateProtocol(Protocol::Chat))
        );
    }

    #[test]
    fn endpoint_without_any_credential_is_rejected() {
        let mut ch = single(Protocol::Chat);
        ch.credential = Credential::default();
        assert_eq!(
            ch.validate(),
            Err(ChannelError::MissingCredential(Protocol::Chat))
        );

        // 端点自带凭据则通过。
        ch.endpoints[0].credential = Some(Credential::new("sk-endpoint"));
        assert_eq!(ch.validate(), Ok(()));
    }

    #[test]
    fn transcode_listing_own_protocol_is_rejected() {
        let mut ch = single(Protocol::Chat);
        ch.endpoints[0].transcode = TranscodePolicy {
            enabled: true,
            accepted: ProtocolSet::from_iter_protocols([Protocol::Chat]),
        };
        assert_eq!(
            ch.validate(),
            Err(ChannelError::SelfTranscode(Protocol::Chat))
        );
    }

    #[test]
    fn empty_name_is_rejected() {
        let mut ch = single(Protocol::Chat);
        ch.name = "   ".into();
        assert_eq!(ch.validate(), Err(ChannelError::EmptyName));
    }

    #[test]
    fn model_entry_validation_catches_empty_names() {
        let mut ch = single(Protocol::Chat);
        ch.endpoints[0].models = vec![ModelEntry::plain("  ")];
        assert_eq!(
            ch.validate(),
            Err(ChannelError::EmptyModelName(Protocol::Chat))
        );

        // 映射到空上游名同样无效（与缺失映射是两码事）。
        let mut ch = single(Protocol::Chat);
        ch.endpoints[0].models = vec![ModelEntry::mapped("my-model", "   ")];
        assert_eq!(
            ch.validate(),
            Err(ChannelError::EmptyUpstreamMapping {
                name: "my-model".into()
            })
        );

        // 正常的映射不受影响。
        let mut ok = single(Protocol::Chat);
        ok.endpoints[0].models = vec![ModelEntry::mapped("my-model", "gpt-4o")];
        assert_eq!(ok.validate(), Ok(()));
    }

    #[test]
    fn unofficial_address_without_base_url_is_rejected() {
        // 端点级非官方地址缺 base_url。
        let mut ch = single(Protocol::Chat);
        ch.endpoints[0].address = UpstreamAddress {
            unofficial: true,
            ..Default::default()
        };
        assert_eq!(
            ch.validate(),
            Err(ChannelError::MissingUnofficialBaseUrl(Protocol::Chat))
        );

        // 渠道默认非官方地址缺 base_url（端点继承它）。
        let mut ch = single(Protocol::Chat);
        ch.address = UpstreamAddress {
            unofficial: true,
            ..Default::default()
        };
        assert_eq!(
            ch.validate(),
            Err(ChannelError::MissingDefaultUnofficialBaseUrl)
        );

        // 带上 base_url 就放行。
        let mut ok = single(Protocol::Chat);
        ok.address = UpstreamAddress {
            unofficial: true,
            base_url: Some("https://relay.example.com".into()),
            ..Default::default()
        };
        assert_eq!(ok.validate(), Ok(()));
    }

    #[test]
    fn extra_header_value_with_crlf_is_rejected() {
        let mut ch = single(Protocol::Chat);
        ch.extra_headers = vec![("x-route".into(), "evil\r\nx-injected: 1".into())];
        assert_eq!(
            ch.validate(),
            Err(ChannelError::InvalidExtraHeaderValue("x-route".into()))
        );

        let mut lf = single(Protocol::Chat);
        lf.extra_headers = vec![("x-route".into(), "a\nb".into())];
        assert_eq!(
            lf.validate(),
            Err(ChannelError::InvalidExtraHeaderValue("x-route".into()))
        );

        let mut ok = single(Protocol::Chat);
        ok.extra_headers = vec![("x-route".into(), "value-ok".into())];
        assert_eq!(ok.validate(), Ok(()));
    }

    #[test]
    fn endpoint_inherits_channel_address_and_credential() {
        let mut ch = single(Protocol::Chat);
        ch.address = UpstreamAddress {
            unofficial: true,
            base_url: Some("https://relay.example.com".into()),
            ..Default::default()
        };
        let ep = &ch.endpoints[0];
        assert_eq!(
            ch.effective_address(ep).base_url.as_deref(),
            Some("https://relay.example.com")
        );
        assert_eq!(ch.effective_credential(ep).expose(), "sk-test-key");
    }

    #[test]
    fn endpoint_overrides_win_over_channel_defaults() {
        let mut ch = single(Protocol::Chat);
        ch.address = UpstreamAddress {
            unofficial: true,
            base_url: Some("https://channel.example.com".into()),
            ..Default::default()
        };
        ch.endpoints[0].address = UpstreamAddress {
            unofficial: true,
            base_url: Some("https://endpoint.example.com".into()),
            ..Default::default()
        };
        ch.endpoints[0].credential = Some(Credential::new("sk-endpoint"));

        let ep = &ch.endpoints[0];
        assert_eq!(
            ch.effective_address(ep).base_url.as_deref(),
            Some("https://endpoint.example.com")
        );
        assert_eq!(ch.effective_credential(ep).expose(), "sk-endpoint");
    }

    #[test]
    fn blank_endpoint_credential_falls_back_to_channel() {
        let mut ch = single(Protocol::Chat);
        ch.endpoints[0].credential = Some(Credential::new("   "));
        assert_eq!(
            ch.effective_credential(&ch.endpoints[0]).expose(),
            "sk-test-key"
        );
    }

    #[test]
    fn endpoints_sort_by_order_and_skip_disabled() {
        let mut ch = single(Protocol::Chat);
        ch.kind = ChannelKind::Aggregate;
        ch.endpoints = vec![
            ChannelEndpoint {
                order: 5,
                ..ChannelEndpoint::new(Protocol::Gemini)
            },
            ChannelEndpoint {
                order: 1,
                ..ChannelEndpoint::new(Protocol::Messages)
            },
            ChannelEndpoint {
                order: 0,
                enabled: false,
                ..ChannelEndpoint::new(Protocol::Chat)
            },
        ];
        let ordered: Vec<Protocol> = ch.endpoints_by_order().iter().map(|e| e.protocol).collect();
        assert_eq!(ordered, vec![Protocol::Messages, Protocol::Gemini]);
    }

    #[test]
    fn exposed_models_dedupe_across_endpoints() {
        let mut ch = single(Protocol::Chat);
        ch.kind = ChannelKind::Aggregate;
        ch.endpoints = vec![
            ChannelEndpoint {
                order: 0,
                models: vec![ModelEntry::plain("claude-sonnet-4-6")],
                ..ChannelEndpoint::new(Protocol::Messages)
            },
            ChannelEndpoint {
                order: 1,
                models: vec![
                    ModelEntry::plain("claude-sonnet-4-6"),
                    ModelEntry::plain("gpt-4o"),
                ],
                ..ChannelEndpoint::new(Protocol::Chat)
            },
        ];
        assert_eq!(ch.exposed_models(), vec!["claude-sonnet-4-6", "gpt-4o"]);
    }

    #[test]
    fn channel_kind_serde_is_flat() {
        let single_json = serde_json::to_string(&ChannelKind::Single(Protocol::Messages)).unwrap();
        assert_eq!(single_json, r#""messages""#);
        let agg_json = serde_json::to_string(&ChannelKind::Aggregate).unwrap();
        assert_eq!(agg_json, r#""aggregate""#);

        assert_eq!(
            serde_json::from_str::<ChannelKind>(r#""gemini""#).unwrap(),
            ChannelKind::Single(Protocol::Gemini)
        );
        assert_eq!(
            serde_json::from_str::<ChannelKind>(r#""aggregate""#).unwrap(),
            ChannelKind::Aggregate
        );
    }
}
