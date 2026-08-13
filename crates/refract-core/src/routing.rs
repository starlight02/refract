//! 路由策略。
//!
//! 定义「怎么在多个候选端点里挑一个」的规则。真正的候选收集与执行在
//! `refract-router` 中；这里只放**纯粹的策略与排序语义**，因为它们是领域规则，
//! 且必须能脱离存储与网络单独测试。

use serde::{Deserialize, Serialize};

/// 候选选择模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionMode {
    /// 同分层内按权重随机。
    #[default]
    WeightedRandom,
    /// 同分层内按顺序轮转（确定性，便于压测与排障）。
    RoundRobin,
    /// 始终取分层内的第一个（确定性，便于调试）。
    First,
}

/// 全局路由策略。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingPolicy {
    /// 「原生优先」开关（需求 6）。
    ///
    /// - `false`：分层键为 `(priority desc)`，与 new-api 语义一致；
    ///   原生性**完全不参与**排序，同优先级的原生与转换端点在同一分层内
    ///   按选择模式（权重/轮转）平等竞争。
    /// - `true`：分层键为 `(native desc, priority desc)`；
    ///   一个低优先级的原生端点会压过高优先级的转换端点。
    pub native_first: bool,
    /// 同分层内的选择模式。
    pub selection: SelectionMode,
    /// 单次请求最多尝试几个候选（含首次）。
    pub max_attempts: u8,
    /// 是否允许在重试时复用同一渠道的其他端点。
    pub retry_same_channel: bool,
}

impl Default for RoutingPolicy {
    fn default() -> Self {
        Self {
            native_first: true,
            selection: SelectionMode::WeightedRandom,
            max_attempts: 3,
            retry_same_channel: false,
        }
    }
}

/// 一个候选端点的排序键。
///
/// 独立成类型是为了让排序规则可测、可解释 —— 路由为什么选了这个渠道，
/// 是运维时最常问的问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RankKey {
    /// 入口协议是否与端点原生协议一致。
    pub native: bool,
    /// 渠道优先级，越大越优先。
    pub priority: i32,
    /// 端点在渠道内的顺序，越小越优先（需求 5）。
    pub endpoint_order: u16,
}

impl RankKey {
    /// 在给定策略下，把排序键折叠成一个可直接比较的元组。
    ///
    /// 返回值按「越大越优先」排列，故 `endpoint_order` 取负。
    ///
    /// 关闭原生优先时原生性**完全不参与**排序 —— 若把它留作次级键，
    /// 「关闭原生优先但原生仍然优先」的矛盾会在 `First` 选择模式下复现。
    pub fn tier(self, native_first: bool) -> (i8, i32, i32) {
        let native_rank = if native_first {
            i8::from(self.native)
        } else {
            0
        };
        (native_rank, self.priority, -(self.endpoint_order as i32))
    }

    /// 两个候选是否属于同一分层（即应当参与同层内的权重随机）。
    pub fn same_tier(self, other: Self, native_first: bool) -> bool {
        let (a_native, a_prio, _) = self.tier(native_first);
        let (b_native, b_prio, _) = other.tier(native_first);
        a_native == b_native && a_prio == b_prio
    }
}

/// 在权重列表中按权重随机挑一个下标。
///
/// 权重为 0 的候选**仍有机会被选中**（按 1 计），否则一个全 0 权重的分层会
/// 无法路由 —— 这是 new-api 上真实出现过的配置陷阱。
pub fn weighted_pick(weights: &[u32], rng: &mut impl rand::RngExt) -> Option<usize> {
    if weights.is_empty() {
        return None;
    }
    let effective: Vec<u64> = weights.iter().map(|&w| u64::from(w.max(1))).collect();
    let total: u64 = effective.iter().sum();
    let mut point = rng.random_range(0..total);
    for (idx, w) in effective.iter().enumerate() {
        if point < *w {
            return Some(idx);
        }
        point -= w;
    }
    Some(effective.len() - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::SmallRng;

    fn key(native: bool, priority: i32, order: u16) -> RankKey {
        RankKey {
            native,
            priority,
            endpoint_order: order,
        }
    }

    #[test]
    fn native_first_beats_priority_when_enabled() {
        // 需求 6 打开时：原生 + 低优先级 应当压过 非原生 + 高优先级。
        let native_low = key(true, 0, 0);
        let transcoded_high = key(false, 100, 0);
        assert!(native_low.tier(true) > transcoded_high.tier(true));
    }

    #[test]
    fn priority_beats_native_when_disabled() {
        // 需求 6 关闭时：回到 new-api 语义，priority 说了算。
        let native_low = key(true, 0, 0);
        let transcoded_high = key(false, 100, 0);
        assert!(transcoded_high.tier(false) > native_low.tier(false));
    }

    #[test]
    fn native_does_not_rank_at_all_when_disabled() {
        // 关闭原生优先后，原生性完全不参与排序：同优先级同 order 的
        // 原生与转换端点必须完全同权，否则 First 模式下会出现
        // 「关了开关但原生仍然优先」的矛盾。
        let native = key(true, 10, 0);
        let transcoded = key(false, 10, 0);
        assert_eq!(native.tier(false), transcoded.tier(false));
    }

    #[test]
    fn lower_endpoint_order_wins_within_same_channel() {
        // 需求 5：同渠道多个端点提供同一模型时，order 小的优先。
        let first = key(true, 0, 0);
        let second = key(true, 0, 5);
        assert!(first.tier(true) > second.tier(true));
    }

    #[test]
    fn same_tier_ignores_endpoint_order() {
        let a = key(true, 10, 0);
        let b = key(true, 10, 7);
        assert!(a.same_tier(b, true));

        let c = key(false, 10, 0);
        assert!(!a.same_tier(c, true), "native flag splits tiers");
        assert!(
            a.same_tier(c, false),
            "native flag must not split tiers when disabled"
        );
    }

    #[test]
    fn weighted_pick_respects_distribution() {
        let mut rng = SmallRng::seed_from_u64(0xC0FFEE);
        let weights = [1_u32, 9];
        let mut hits = [0_usize; 2];
        for _ in 0..10_000 {
            hits[weighted_pick(&weights, &mut rng).unwrap()] += 1;
        }
        // 期望约 1:9，给足容差但要能抓出"权重被忽略"这类 bug。
        assert!(hits[0] > 600 && hits[0] < 1_400, "hits = {hits:?}");
        assert!(hits[1] > 8_600 && hits[1] < 9_400, "hits = {hits:?}");
    }

    #[test]
    fn weighted_pick_treats_zero_weight_as_one() {
        // 全零权重不能导致无法路由。
        let mut rng = SmallRng::seed_from_u64(7);
        let weights = [0_u32, 0, 0];
        let mut seen = [false; 3];
        for _ in 0..500 {
            seen[weighted_pick(&weights, &mut rng).unwrap()] = true;
        }
        assert_eq!(seen, [true, true, true]);
    }

    #[test]
    fn weighted_pick_on_empty_is_none() {
        let mut rng = SmallRng::seed_from_u64(1);
        assert_eq!(weighted_pick(&[], &mut rng), None);
    }

    #[test]
    fn weighted_pick_single_candidate_is_deterministic() {
        let mut rng = SmallRng::seed_from_u64(3);
        for _ in 0..50 {
            assert_eq!(weighted_pick(&[5], &mut rng), Some(0));
        }
    }

    #[test]
    fn default_policy_prefers_native() {
        let policy = RoutingPolicy::default();
        assert!(policy.native_first);
        assert_eq!(policy.selection, SelectionMode::WeightedRandom);
        assert_eq!(policy.max_attempts, 3);
    }
}
