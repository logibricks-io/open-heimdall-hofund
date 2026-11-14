//! Cross-vendor subscription conformance over recorded fixtures only.

#[path = "subscription_conformance/harness.rs"]
mod harness;
#[path = "subscription_conformance/matrix.rs"]
mod matrix;
#[path = "subscription_conformance/vendors/mod.rs"]
mod vendors;

#[tokio::test(flavor = "current_thread")]
async fn cross_vendor_conformance_matrix() {
    let cells = matrix::cells();
    assert_eq!(cells.len(), 240);
    assert_eq!(
        cells.iter().filter(|(_, supported)| *supported).count(),
        matrix::EXECUTED_CELLS
    );
    assert_eq!(
        cells.iter().filter(|(_, supported)| !supported).count(),
        matrix::NA_CELLS
    );

    let documentation = include_str!("../../../docs/plugins/subscription-conformance.md");
    assert!(
        documentation.contains(&matrix::render()),
        "the documented matrix differs from the executable declaration"
    );

    for (cell, supported) in cells {
        if supported {
            matrix::execute(cell)
                .await
                .unwrap_or_else(|error| panic!("{}: {error}", cell.label()));
        }
    }
}
