//! Embeds the web console.
//!
//! The console is a TypeScript + React app in `web/console`, built by Vite
//! into a single `dist/index.html` with its scripts and styles inlined. That
//! file is what `sandstormd` serves at `/`. It is built output, so it is not
//! in the repository: CI, releases and the Docker image build it before
//! cargo does. A plain `cargo build` without it still works and serves a page
//! that says how to build the console, so working on the Rust side never
//! needs Node.

use std::path::PathBuf;

const UNBUILT: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sandstorm console</title>
<style>body{font:15px/1.6 system-ui,sans-serif;max-width:560px;margin:12vh auto;padding:0 16px;color:#222;background:#fff}
code{background:#f2f2f2;padding:2px 5px;border-radius:4px}
@media (prefers-color-scheme:dark){body{color:#eee;background:#111}code{background:#222}}</style></head>
<body><h1>The console was not built into this binary</h1>
<p>This <code>sandstormd</code> was compiled without the web console. Build it, then build the binary again:</p>
<p><code>npm --prefix web/console ci &amp;&amp; npm --prefix web/console run build</code></p>
<p>The API is unaffected and answers as usual.</p></body></html>
"#;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let dist = manifest.join("../../web/console/dist");
    let page = dist.join("index.html");
    // The directory as well as the file: a build that has not happened yet
    // creates both, and cargo has to notice either.
    println!("cargo:rerun-if-changed={}", dist.display());
    println!("cargo:rerun-if-changed={}", page.display());
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("set by cargo")).join("console.html");
    let body = match std::fs::read_to_string(&page) {
        Ok(built) => built,
        Err(_) => {
            println!(
                "cargo:warning=web/console is not built; sandstormd will serve a page that says how to build it"
            );
            UNBUILT.to_string()
        }
    };
    std::fs::write(&out, body).expect("writing the console into OUT_DIR");
}
