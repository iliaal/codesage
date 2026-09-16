use visibility_fixture::store::{crate_only, open_public};

#[test]
fn exercises_public_surface() {
    let total = open_public() + crate_only();
    assert_eq!(total, 5);
}
