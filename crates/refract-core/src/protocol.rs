//! 协议标识。
//!
//! [`Protocol`] 同时扮演三种角色：
//! 1. **入口协议** —— 客户端请求打到网关时使用的协议，由 HTTP 路径决定。
//! 2. **渠道原生协议** —— 上游端点真正说的协议。
//! 3. **转换目标** —— 协议转换开关中被勾选的协议。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// 网关支持的四种 LLM 协议。
///
/// 这个枚举是整个系统的基石：渠道类型、入口路由、协议转换策略全部围绕它展开。
/// 刻意**不包含**厂商概念 —— 厂商差异通过 [`crate::UpstreamAddress`] 与凭据表达。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// OpenAI Chat Completions：`POST /v1/chat/completions`
    Chat,
    /// OpenAI Responses API：`POST /v1/responses`
    Responses,
    /// Anthropic Messages API：`POST /v1/messages`
    Messages,
    /// Google Gemini：`POST /v1beta/models/{model}:generateContent`
    Gemini,
}

impl Protocol {
    /// 全部协议，顺序稳定（用于 UI 展示与遍历）。
    pub const ALL: [Protocol; 4] = [
        Protocol::Chat,
        Protocol::Responses,
        Protocol::Messages,
        Protocol::Gemini,
    ];

    /// 稳定的字符串标识，用于配置文件、数据库与 API。
    pub const fn as_str(self) -> &'static str {
        match self {
            Protocol::Chat => "chat",
            Protocol::Responses => "responses",
            Protocol::Messages => "messages",
            Protocol::Gemini => "gemini",
        }
    }

    /// 人类可读名称。
    pub const fn display_name(self) -> &'static str {
        match self {
            Protocol::Chat => "OpenAI Chat Completions",
            Protocol::Responses => "OpenAI Responses",
            Protocol::Messages => "Anthropic Messages",
            Protocol::Gemini => "Google Gemini",
        }
    }

    /// 该协议官方默认的 base URL（不含版本前缀与路径）。
    pub const fn default_base_url(self) -> &'static str {
        match self {
            Protocol::Chat | Protocol::Responses => "https://api.openai.com",
            Protocol::Messages => "https://api.anthropic.com",
            Protocol::Gemini => "https://generativelanguage.googleapis.com",
        }
    }

    /// 该协议官方默认的版本前缀。
    pub const fn default_version_prefix(self) -> &'static str {
        match self {
            Protocol::Chat | Protocol::Responses | Protocol::Messages => "/v1",
            Protocol::Gemini => "/v1beta",
        }
    }

    /// 该协议官方默认的推理端点路径（版本前缀之后的部分）。
    ///
    /// Gemini 的路径含 `{model}` 与动作后缀，由地址解析阶段替换。
    pub const fn default_path(self) -> &'static str {
        match self {
            Protocol::Chat => "/chat/completions",
            Protocol::Responses => "/responses",
            Protocol::Messages => "/messages",
            Protocol::Gemini => "/models/{model}:{action}",
        }
    }

    /// 该协议官方默认的模型列表路径（版本前缀之后的部分）。
    pub const fn default_models_path(self) -> &'static str {
        match self {
            Protocol::Chat | Protocol::Responses | Protocol::Messages => "/models",
            Protocol::Gemini => "/models",
        }
    }

    /// 该协议的凭据注入方式。
    pub const fn auth_scheme(self) -> AuthScheme {
        match self {
            Protocol::Chat | Protocol::Responses => AuthScheme::Bearer,
            Protocol::Messages => AuthScheme::AnthropicApiKey,
            Protocol::Gemini => AuthScheme::GoogleApiKey,
        }
    }

    /// 该协议的路径是否随模型名变化。
    ///
    /// 仅 Gemini 为真：它把模型名与动作编码在 URL 里，而非请求体。
    pub const fn path_is_model_dependent(self) -> bool {
        matches!(self, Protocol::Gemini)
    }

    /// 判断一个 URL 路径是否像该协议的推理端点。
    ///
    /// 用于「非官方」模式下的路径校验（[`crate::UpstreamAddress`] 未开启完整地址时）。
    /// 刻意宽松：只要求路径**以**协议特征片段结尾，允许任意前缀（中转站常加 `/proxy/xxx`）。
    pub fn path_looks_native(self, path: &str) -> bool {
        let path = path.trim_end_matches('/');
        match self {
            Protocol::Chat => path.ends_with("/chat/completions"),
            Protocol::Responses => path.ends_with("/responses"),
            Protocol::Messages => path.ends_with("/messages"),
            Protocol::Gemini => {
                path.contains(":generateContent")
                    || path.contains(":streamGenerateContent")
                    || path.contains("/models/")
            }
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 解析 [`Protocol`] 失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown protocol: {0}")]
pub struct ParseProtocolError(pub String);

impl FromStr for Protocol {
    type Err = ParseProtocolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "chat" => Ok(Protocol::Chat),
            "responses" | "res" => Ok(Protocol::Responses),
            "messages" | "message" => Ok(Protocol::Messages),
            "gemini" => Ok(Protocol::Gemini),
            other => Err(ParseProtocolError(other.to_owned())),
        }
    }
}

/// 上游凭据的注入方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthScheme {
    /// `Authorization: Bearer <key>`
    Bearer,
    /// `x-api-key: <key>` + `anthropic-version: <date>`
    AnthropicApiKey,
    /// `x-goog-api-key: <key>`
    GoogleApiKey,
}

/// 一个协议集合。
///
/// 用 `u8` 位图实现，避免为「勾选了哪些协议」分配堆内存 —— 协议转换策略是热路径上
/// 每个请求都要查的东西。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ProtocolSet(u8);

impl ProtocolSet {
    /// 空集合。
    pub const EMPTY: Self = Self(0);

    /// 含全部四种协议。
    pub const ALL: Self = Self(0b1111);

    const fn bit(p: Protocol) -> u8 {
        1 << (p as u8)
    }

    /// 由若干协议构造。
    pub fn from_iter_protocols<I: IntoIterator<Item = Protocol>>(iter: I) -> Self {
        let mut set = Self::EMPTY;
        for p in iter {
            set.insert(p);
        }
        set
    }

    /// 加入一个协议。
    pub fn insert(&mut self, p: Protocol) {
        self.0 |= Self::bit(p);
    }

    /// 移除一个协议。
    pub fn remove(&mut self, p: Protocol) {
        self.0 &= !Self::bit(p);
    }

    /// 是否包含。
    pub const fn contains(self, p: Protocol) -> bool {
        self.0 & Self::bit(p) != 0
    }

    /// 是否为空。
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// 元素个数。
    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }

    /// 迭代其中的协议，顺序与 [`Protocol::ALL`] 一致。
    pub fn iter(self) -> impl Iterator<Item = Protocol> {
        Protocol::ALL.into_iter().filter(move |p| self.contains(*p))
    }
}

impl fmt::Display for ProtocolSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for p in self.iter() {
            if !first {
                f.write_str(",")?;
            }
            f.write_str(p.as_str())?;
            first = false;
        }
        Ok(())
    }
}

impl Serialize for ProtocolSet {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.collect_seq(self.iter())
    }
}

impl<'de> Deserialize<'de> for ProtocolSet {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let items = Vec::<Protocol>::deserialize(de)?;
        Ok(Self::from_iter_protocols(items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_roundtrips_through_str() {
        for p in Protocol::ALL {
            assert_eq!(p.as_str().parse::<Protocol>().unwrap(), p);
        }
    }

    #[test]
    fn protocol_accepts_user_facing_aliases() {
        // 需求里管它们叫 "res 协议" 与 "message 协议"。
        assert_eq!("res".parse::<Protocol>().unwrap(), Protocol::Responses);
        assert_eq!("message".parse::<Protocol>().unwrap(), Protocol::Messages);
    }

    #[test]
    fn unknown_protocol_is_rejected() {
        assert!("bedrock".parse::<Protocol>().is_err());
    }

    #[test]
    fn protocol_set_tracks_membership() {
        let mut set = ProtocolSet::from_iter_protocols([Protocol::Chat, Protocol::Gemini]);
        assert!(set.contains(Protocol::Chat));
        assert!(set.contains(Protocol::Gemini));
        assert!(!set.contains(Protocol::Messages));
        assert_eq!(set.len(), 2);

        set.remove(Protocol::Chat);
        assert!(!set.contains(Protocol::Chat));
        assert_eq!(set.len(), 1);

        set.insert(Protocol::Messages);
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            vec![Protocol::Messages, Protocol::Gemini]
        );
    }

    #[test]
    fn protocol_set_all_contains_everything() {
        assert_eq!(ProtocolSet::ALL.len(), 4);
        for p in Protocol::ALL {
            assert!(ProtocolSet::ALL.contains(p));
        }
        assert!(ProtocolSet::EMPTY.is_empty());
    }

    #[test]
    fn protocol_set_serde_is_a_json_array() {
        let set = ProtocolSet::from_iter_protocols([Protocol::Responses, Protocol::Chat]);
        let json = serde_json::to_string(&set).unwrap();
        // 序列化顺序跟随 Protocol::ALL，而非插入顺序。
        assert_eq!(json, r#"["chat","responses"]"#);
        assert_eq!(serde_json::from_str::<ProtocolSet>(&json).unwrap(), set);
    }

    #[test]
    fn native_path_detection_allows_proxy_prefixes() {
        assert!(Protocol::Chat.path_looks_native("/v1/chat/completions"));
        assert!(Protocol::Chat.path_looks_native("/relay/openai/v1/chat/completions"));
        assert!(!Protocol::Chat.path_looks_native("/v1/messages"));
        assert!(Protocol::Messages.path_looks_native("/v1/messages"));
        assert!(
            Protocol::Gemini
                .path_looks_native("/v1beta/models/gemini-2.5-pro:streamGenerateContent")
        );
    }

    #[test]
    fn only_gemini_encodes_model_in_path() {
        assert!(Protocol::Gemini.path_is_model_dependent());
        for p in [Protocol::Chat, Protocol::Responses, Protocol::Messages] {
            assert!(!p.path_is_model_dependent());
        }
    }
}
