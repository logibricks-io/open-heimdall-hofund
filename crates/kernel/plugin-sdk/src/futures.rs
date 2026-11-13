//! Small future helpers re-exported for plugins.

pub use futures_util::stream::{FuturesUnordered, Stream, StreamExt, empty, unfold};

/// Returns the next completed future in `futures`.
///
/// Unlike `StreamExt::next`, this stays pending forever while the set is empty,
/// so an empty append window cannot starve another branch of a race.
pub async fn next_ready<F>(futures: &mut FuturesUnordered<F>) -> F::Output
where
    F: std::future::Future,
{
    use futures_util::StreamExt;

    if futures.is_empty() {
        return std::future::pending().await;
    }
    match futures.next().await {
        Some(output) => output,
        None => std::future::pending().await,
    }
}

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

    use super::{FuturesUnordered, Race, next_ready, race};

    #[tokio::test]
    async fn next_ready_pends_while_empty_then_returns_a_result() {
        let mut futures = FuturesUnordered::<std::future::Ready<u8>>::new();
        assert_eq!(
            race(next_ready(&mut futures), async { 7_u8 }).await,
            Race::Right(7)
        );
        futures.push(std::future::ready(9));
        assert_eq!(next_ready(&mut futures).await, 9);
    }

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
