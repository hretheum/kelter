//! Mass-load harness: install every skill from a bundle directory into a
//! throwaway registry and report the numbers — the "anyone adds any skill,
//! no special-casing" proof (brief §7.5).
//!
//!     cargo run -p skills --example load-bundle -- <bundle-dir>

fn main() {
    let src = std::env::args().nth(1).expect("usage: load-bundle <dir>");
    let root = std::env::temp_dir().join(format!("kelter-skill-bundle-{}", std::process::id()));
    let reg = skills::SkillRegistry::open(&root).expect("registry");
    let started = std::time::Instant::now();
    let report = reg
        .install_bundle(std::path::Path::new(&src))
        .expect("bundle install");
    let listed = reg.list();
    let untrusted = listed
        .iter()
        .filter(|s| s.provenance.trust == "untrusted")
        .count();
    let with_scripts = listed.iter().filter(|s| s.has_scripts).count();
    let catalog = reg.catalog();
    println!(
        "installed={} skipped={} listed={} untrusted={} with_scripts(candidates)={} \
         catalog_bytes={} (cap {}) elapsed={:?}",
        report.installed.len(),
        report.skipped.len(),
        listed.len(),
        untrusted,
        with_scripts,
        catalog.len(),
        skills::MAX_CATALOG_BYTES,
        started.elapsed(),
    );
    for (dir, why) in report.skipped.iter().take(10) {
        println!("  skipped: {dir}: {why}");
    }
    // Show three sample catalog lines (discovery tier — no bodies).
    for line in catalog.lines().take(3) {
        println!("  {line}");
    }
    let _ = std::fs::remove_dir_all(&root);
}
