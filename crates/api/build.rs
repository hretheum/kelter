//! `src/lib.rs` embeds `apps/web-ui/dist` (the Vite build output) with
//! rust-embed. `dist/` is gitignored, so on a fresh checkout — notably
//! `cargo install --git https://github.com/hretheum/kelter daemon` — the folder
//! does not exist and the derive fails with an opaque `E0599` after minutes of
//! compiling dependencies.
//!
//! This script makes that case work or fail early and readably:
//! - `dist/index.html` present (CI, a local `pnpm build`): does nothing.
//! - absent and `pnpm` (or `corepack`) is available: builds the UI.
//! - absent and no pnpm: stops with instructions, before anything else compiles
//!   in this crate.
//!
//! A stub UI was rejected on purpose: it would produce a daemon that builds
//! green and serves an empty page.

use std::path::Path;
use std::process::Command;

fn main() {
    let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/web-ui");
    let index = ui.join("dist/index.html");

    // Re-run when the build output appears/disappears, and when the lockfile
    // changes (so a stale dist is not the script's concern, only a missing one).
    println!("cargo:rerun-if-changed={}", index.display());
    println!("cargo:rerun-if-changed=build.rs");

    if index.is_file() {
        return;
    }
    if !ui.join("package.json").is_file() {
        // Not a full checkout (e.g. a packaged crate): nothing we can build.
        fail(&ui, "apps/web-ui/package.json not found");
    }

    let Some(pnpm) = pnpm_command() else {
        fail(&ui, "pnpm was not found (and neither was corepack)");
    };

    println!(
        "cargo:warning=apps/web-ui/dist missing — building the web UI with `{}`",
        pnpm.join(" ")
    );
    for args in [&["install", "--frozen-lockfile"][..], &["build"][..]] {
        let ok = Command::new(&pnpm[0])
            .args(&pnpm[1..])
            .args(args)
            .current_dir(&ui)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            fail(
                &ui,
                &format!("`{} {}` failed", pnpm.join(" "), args.join(" ")),
            );
        }
    }
    if !index.is_file() {
        fail(
            &ui,
            "the UI build finished but dist/index.html is still missing",
        );
    }
}

/// `["pnpm"]`, or `["corepack", "pnpm"]` when only corepack is installed.
fn pnpm_command() -> Option<Vec<String>> {
    let works = |prog: &str, pre: &[&str]| {
        Command::new(prog)
            .args(pre)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if works("pnpm", &[]) {
        Some(vec!["pnpm".into()])
    } else if works("corepack", &["pnpm"]) {
        Some(vec!["corepack".into(), "pnpm".into()])
    } else {
        None
    }
}

fn fail(ui: &Path, why: &str) -> ! {
    panic!(
        "\n\nKelter embeds its web UI into the binary, and it is not built: {why}.\n\
         Build it once, then re-run cargo:\n\n    \
         pnpm --dir {ui} install --frozen-lockfile && pnpm --dir {ui} build\n\n\
         (needs Node 22 + pnpm; see docs/INSTALL.md — or install a prebuilt binary \
         from the GitHub Releases page instead)\n",
        ui = ui.display()
    )
}
