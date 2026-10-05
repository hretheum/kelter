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

#[test]
fn ask_timeout_zero_is_refused_at_startup() {
    // A pause must always end: 0 ("wait forever") is a config error.
    let cfg = parse("[approval]\nask_timeout_secs = 0\n").unwrap();
    let err = cfg.approval.validate().unwrap_err().to_string();
    assert!(err.contains("ask_timeout_secs must be > 0"), "{err}");
    // Config::load is the path the daemon takes, so it refuses too.
    let dir = std::env::temp_dir().join(format!("kelter-gov-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("zero.toml");
    std::fs::write(
        &path,
        "[policy]\nengine = \"local:x\"\n[approval]\nask_timeout_secs = 0\n",
    )
    .unwrap();
    assert!(Config::load(path.to_str().unwrap()).is_err());
    std::fs::write(
        &path,
        "[policy]\nengine = \"local:x\"\n[approval]\nask_timeout_secs = 1\n",
    )
    .unwrap();
    assert!(Config::load(path.to_str().unwrap()).is_ok());
    std::fs::remove_dir_all(&dir).ok();
    // The default is valid.
    parse("").unwrap().approval.validate().unwrap();
}

#[test]
fn a_policy_on_a_non_mutating_action_is_refused_at_startup() {
    use kernel_core::{ActionSchema, ActionSpec};
    let spec = |name: &str, props: serde_json::Value| ActionSpec {
        name: name.into(),
        description: String::new(),
        parameters: serde_json::json!({"type": "object", "properties": props}),
    };
    let schema = ActionSchema {
        actions: vec![
            spec(
                "czytaj_plik",
                serde_json::json!({"path": {"type": "string"}}),
            ),
            spec(
                "zastosuj_patch",
                serde_json::json!({"patch": {"type": "string"}, "idempotency_key": {"type": "string"}}),
            ),
            spec(
                "delegate_to_reviewer",
                serde_json::json!({"task": {"type": "string"}}),
            ),
        ],
    };
    // Mutating entry, review_verdict, and a name the registry lacks: start (the
    // unknown name is returned for a WARN).
    let ok = parse(
        "[approval.actions]\nzastosuj_patch = \"ask\"\nreview_verdict = \"ask\"\nzniknal = \"deny\"\n",
    )
    .unwrap();
    assert_eq!(
        ok.approval.check_against(&schema).unwrap(),
        vec!["zniknal".to_string()]
    );
    // A read-only tool or a delegation action under a policy: refused, both named.
    for extra in [
        "[approval.actions]\nczytaj_plik = \"deny\"\n",
        "[approval.actions]\ndelegate_to_reviewer = \"ask\"\nzastosuj_patch = \"ask\"\n",
    ] {
        let cfg = parse(extra).unwrap();
        let err = cfg.approval.check_against(&schema).unwrap_err().to_string();
        assert!(err.contains("non-mutating"), "{err}");
        assert!(
            err.contains("czytaj_plik") || err.contains("delegate_to_reviewer"),
            "{err}"
        );
        assert!(!err.contains("zastosuj_patch"), "{err}");
    }
}
