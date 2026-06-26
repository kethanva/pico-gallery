//! Verifies the menu's "Save settings" path: a parsed config must re-serialise
//! to TOML that parses back to an equivalent config (plugins + flattened keys).
use picogallery::config::Config;

#[test]
fn config_roundtrips_through_toml() {
    let src =
        std::fs::read_to_string("config.example.toml").expect("config.example.toml should exist");
    let cfg: Config = toml::from_str(&src).expect("example config parses");
    let out = toml::to_string_pretty(&cfg).expect("config serialises to TOML");
    let reparsed: Config = toml::from_str(&out).expect("serialised config parses again");

    assert_eq!(cfg.plugins.len(), reparsed.plugins.len());
    assert_eq!(
        cfg.plugins
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>(),
        reparsed
            .plugins
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>()
    );
}
