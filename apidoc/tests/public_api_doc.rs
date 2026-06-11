//! Public-API surface snapshots for the PARENT package (docs/public-api/).
//! Shared implementation + format docs: the `zenutils-apidoc` crate.
//!
//! zenflate uses the default configuration: supported surface = default
//! features; features file = all manifest features except `_*`-prefixed
//! internal gates — the same selection as the pre-runner snapshot test.
#[test]
fn public_api_surface_docs_are_current() {
    zenutils_apidoc::ApiDoc::new().workspace_dir("..").run();
}
