//! Claude Code bridge state, stream-json codec, and tool parking.

pub mod decode;
pub mod park;
pub mod run;
pub mod wsl;

#[cfg(test)]
pub(crate) mod runner;

pub use park::Bridge;

#[cfg(test)]
mod tests {
    use super::Bridge;

    #[test]
    fn a_new_bridge_is_empty() {
        let _bridge = Bridge::new();
    }
}
