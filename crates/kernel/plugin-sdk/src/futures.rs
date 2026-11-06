//! Small future helpers re-exported for plugins.

pub use futures_util::stream::FuturesUnordered;

/// Returns whichever future completes first.
pub async fn race<A, B>(left: A, right: B) -> Race<A::Output, B::Output>
where
    A: std::future::Future,
    B: std::future::Future,
{
    tokio::pin!(left);
    tokio::pin!(right);
    tokio::select! {
        left = &mut left => Race::Left(left),
        right = &mut right => Race::Right(right),
    }
}

/// The branch that won a [`race`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Race<L, R> {
    /// The first future finished first.
    Left(L),
    /// The second future finished first.
    Right(R),
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Race, race};

    #[tokio::test(start_paused = true)]
    async fn race_returns_the_branch_that_finishes_first() {
        let left = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            1_u8
        };
        let right = async { 2_u8 };
        assert_eq!(race(left, right).await, Race::Right(2));
    }

    #[tokio::test(start_paused = true)]
    async fn inverted_race_branch_fails_when_select_is_swapped() {
        let left = async { 9_u8 };
        let right = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            8_u8
        };
        assert_ne!(race(left, right).await, Race::Right(9));
    }
}
