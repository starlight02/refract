//! 声明式路由组合工具。
//!
//! 提供宏与集合组合器，在编译期与构建期折叠 warp 过滤器链，
//! 消除 `warp` 因泛型深度嵌套引发的类型检查器爆炸与繁琐样板代码。

use warp::filters::BoxedFilter;
use warp::{Filter, Reply};

/// 声明式组合多个同构的 warp 路由。
///
/// 接受任意数量的 Filter（支持未装箱的 `impl Filter` 或已装箱的 `BoxedFilter`），
/// 在编译期自动展开为平坦折叠链，并在每一步通过 `.boxed()` 擦除类型。
///
/// # 示例
/// ```ignore
/// let api = routes![
///     get_policy,
///     set_policy,
///     get_retention,
///     set_retention,
/// ];
/// ```
#[macro_export]
macro_rules! routes {
    ($head:expr $(,)?) => {
        warp::Filter::boxed($head)
    };
    ($head:expr, $($tail:expr),+ $(,)?) => {
        $head
            $( .or($tail).unify().boxed() )+
    };
}

/// 将动态集合或数组中的同构 `BoxedFilter` 聚合为单一路由。
///
/// 当路由清单在运行时动态计算、或需要通过迭代器流水线组装时使用；
/// 静态已知路由推荐直接使用 [`routes!`] 宏以获得最佳编译期内联。
pub fn any_of<T>(routes: impl IntoIterator<Item = BoxedFilter<(T,)>>) -> BoxedFilter<(T,)>
where
    T: Reply + Send + 'static,
{
    let mut iter = routes.into_iter();
    let first = iter.next().expect("any_of requires at least one route");
    iter.fold(first, |acc, next| acc.or(next).unify().boxed())
}
