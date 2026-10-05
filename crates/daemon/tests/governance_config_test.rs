//! `[approval]` (ADR 0038) and `[budget]` (ADR 0039) config: absent = inert,
//! the documented example parses, and a mistake fails closed at startup.

use daemon::config::Config;
use kernel_core::ActionPolicy;

fn parse(extra: &str) -> anyhow::Result<Config> {
    let raw = format!("[policy]\nengine = \"local:x\"\n{extra}");
    Ok(toml::from_str(&raw)?)
}

#[test]
fn absent_sections_are_inert() {
    let cfg = parse("").unwrap();
    let gate = cfg.approval.to_gate();
    assert_eq!(gate.default, ActionPolicy::Allow);
    assert!(gate.actions.is_empty() && !gate.can_ask());
    assert!(cfg.budget.to_budget().unwrap().is_inert());
}

#[test]
fn the_example_config_blocks_parse_when_uncommented() {
    // Uncomment exactly the [approval]/[budget] blocks of config.example.toml.
    let example = include_str!("../../../config.example.toml");
    let mut block = String::new();
    let mut on = false;
    for line in example.lines() {
        let l = line.trim_start_matches("# ");
        if l.starts_with("[approval") || l.starts_with("[budget]") {
            on = true;
        } else if line.starts_with('[') || line.is_empty() {
            on = false;
        }
        if on && line.starts_with("# ") {
            block.push_str(l);
            block.push('\n');
        }
    }
    let cfg = parse(&block).unwrap();
    let gate = cfg.approval.to_gate();
    assert_eq!(gate.actions["zastosuj_patch"], ActionPolicy::Ask);
    assert_eq!(gate.actions["uruchom_komende"], ActionPolicy::Deny);
    assert!(gate.review_by_human());
    let b = cfg.budget.to_budget().unwrap();
    assert_eq!(b.per_run.max_tokens, Some(200_000));
    assert_eq!(b.daily_usd_per_principal, Some(5.0));
}

#[test]
fn mistakes_fail_closed() {
    // A typo'd policy is an error, not a silent allow.
    assert!(parse("[approval.actions]\nwrite_file = \"aks\"\n").is_err());
    assert!(parse("[approval]\ndefualt = \"deny\"\n").is_err());
    assert!(parse("[budget]\nmax_dollars = 1.0\n").is_err());
    let cfg = parse("[budget]\nmax_usd = -1.0\n").unwrap();
    assert!(cfg.budget.to_budget().is_err());
    let cfg = parse("[budget]\nmax_tokens = 0\n").unwrap();
    assert!(cfg.budget.to_budget().is_err());
}
