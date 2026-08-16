//! Serving the files in `assets/`.
//!
//! Read from disk so editing a page and reloading is enough, with the compiled-in copy
//! as the fallback so a binary carried to a venue still works. That arrangement already
//! existed for the one page there used to be; it is here because it now has to happen
//! four times, and it was written out twice each time — `include_str!` cannot take a
//! `const`, so the path appeared once as a runtime string and once as a literal, which
//! is exactly the shape of bug where the two drift apart and nobody notices because the
//! fallback silently serves the stale one.
//!
//! That bug has already happened once on this project: `ASSET_PATH` was repo-relative,
//! the disk read failed for every request, and every edit to the page appeared to do
//! nothing until the next rebuild.

/// A `GET` handler serving one file from `assets/`, disk first.
///
/// ```ignore
/// .route("/app.css", asset_route!("app.css", "text/css; charset=utf-8"))
/// ```
#[macro_export]
macro_rules! asset_route {
    ($file:literal, $mime:literal) => {
        axum::routing::get(|| async {
            // Both paths absolute, from the crate root. `include_str!` resolves
            // relative to the file that *invokes* the macro, so a relative fallback
            // would mean a different path per call site — which is how the original
            // arrangement ended up pointing at nothing and silently serving the
            // compiled-in copy for every request.
            let body = std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/",
                $file
            ))
            .unwrap_or_else(|_| {
                include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/", $file)).to_string()
            });
            (
                [
                    (axum::http::header::CONTENT_TYPE, $mime),
                    // The page is edited while the server runs. A cached copy is a
                    // change that did not happen.
                    (
                        axum::http::header::CACHE_CONTROL,
                        "no-store, must-revalidate",
                    ),
                ],
                body,
            )
        })
    };
}
