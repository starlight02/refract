//! 路由层。
//!
//! 职责：给定「客户端要的模型 + 入口协议」，返回「打哪个上游、用哪个协议、
//! 需不需要转码」。这是需求 4（协议转换）、5（端点优先级）、6（原生优先）
//! 三个需求交汇的地方。
//!
//! ## 为什么分 planner + executor 两层
//!
//! - **Planner** 是纯函数：输入一组候选 + 策略，输出排好序的路由计划。可以
//!   离线跑、单元测试不需要 mock 任何外部状态。
//! - **Executor** 处理网络与状态：执行一个候选、失败后取下一个候选重试、记录
//!   健康度。它是有状态的，需要上游客户端和健康仓储。
//!
//! 两层的分离意味着路由决策的逻辑可以被独立审计：「为什么这个请求打了这个
//! 渠道」只取决于 planner 的输出，不混杂重试的副作用。

// lint 配置统一在 workspace `Cargo.toml` 的 [workspace.lints] 里维护。

pub mod executor;
pub mod plan;

pub use executor::{
    InboundPayload, RouteExecutor, RouteOutcome, RoutedResponse, RoutedStream, RouterConfig,
};
pub use plan::{Candidate, Diagnosis, RoundRobinCursors, Route, RoutePlanner};
