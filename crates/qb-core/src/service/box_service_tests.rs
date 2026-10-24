use super::*;
use crate::boxes::BoxName;

fn fixture(declared: &[&str]) -> (tempfile::TempDir, BoxRegistry, BoxService) {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".sandbox/tests/box-service-selection");
    std::fs::create_dir_all(&base).unwrap();
    let home = tempfile::tempdir_in(base).unwrap();
    let registry = BoxRegistry::open(home.path()).unwrap();
    let service = BoxService {
        sockets: SocketSet::new(),
        registry: Some(registry.clone()),
        declared: declared.iter().map(|name| (*name).to_owned()).collect(),
        visible_port: 10200,
        start_attempted: false,
        stopping: false,
        failed: false,
    };
    (home, registry, service)
}

fn enroll(registry: &BoxRegistry, name: &str) -> BoxScope {
    registry
        .add(BoxName::parse(name).unwrap())
        .unwrap()
        .into_parts()
        .0
}

#[test]
fn listener_selection_requires_exact_declaration_without_removing_key_identity() {
    let (_home, registry, service) = fixture(&["bound", "not-enrolled"]);
    let bound = enroll(&registry, "bound");
    let key_only = enroll(&registry, "bound-extra");
    assert_eq!(service.listener_scopes().unwrap(), vec![bound.clone()]);
    assert_eq!(
        service.active_scopes().unwrap(),
        vec![bound, key_only.clone()]
    );

    let dynamic = enroll(&registry, "dynamic");
    assert_eq!(service.listener_scopes().unwrap().len(), 1);
    assert!(service.active_scopes().unwrap().contains(&dynamic));
    assert!(service.active_scopes().unwrap().contains(&key_only));
}

#[test]
fn listener_selection_uses_fresh_active_epoch_and_empty_declarations_bind_nothing() {
    let (_home, registry, mut service) = fixture(&[]);
    let old = enroll(&registry, "bound");
    assert!(service.listener_scopes().unwrap().is_empty());
    assert_eq!(service.active_scopes().unwrap(), vec![old.clone()]);

    service.declared.push("bound".to_owned());
    assert_eq!(service.listener_scopes().unwrap(), vec![old.clone()]);
    registry.revoke(old.name()).unwrap();
    assert!(service.listener_scopes().unwrap().is_empty());
    let next = enroll(&registry, "bound");
    assert_ne!(next, old);
    assert_eq!(service.listener_scopes().unwrap(), vec![next]);
}

#[test]
fn name_only_configuration_keeps_identity_without_declaring_a_listener() {
    let config: Config =
        toml::from_str("[[boxes]]\nname='key-only'\n[[boxes]]\nname='bound'\nlisten='unix'\n")
            .unwrap();
    config.validate_boxes().unwrap();
    let declared = declared_listeners(&config);
    assert_eq!(declared, ["bound"]);
    let names: Vec<_> = declared.iter().map(String::as_str).collect();
    let (_home, registry, service) = fixture(&names);
    let key_only = enroll(&registry, "key-only");
    let bound = enroll(&registry, "bound");
    assert_eq!(service.listener_scopes().unwrap(), vec![bound]);
    assert!(service.active_scopes().unwrap().contains(&key_only));
    assert!(declared_listeners(&Config::default()).is_empty());
    let only: Config = toml::from_str("[[boxes]]\nname='key-only'\n").unwrap();
    assert!(declared_listeners(&only).is_empty());
}

#[test]
fn explicit_tcp_listener_is_refused_before_registry_io() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".sandbox/tests/box-service-selection");
    std::fs::create_dir_all(&base).unwrap();
    let parent = tempfile::tempdir_in(base).unwrap();
    let home = parent.path().join("uncreated-home");
    assert!(!home.try_exists().unwrap());
    let config: Config =
        toml::from_str("[[boxes]]\nname='tcp-only'\nlisten='tcp:127.0.0.1:10301'\n").unwrap();
    config.validate_boxes().unwrap();

    let error = match BoxService::prepare(&home, &config) {
        Err(error) => error,
        Ok(_) => panic!("explicit TCP listener must be refused"),
    };
    assert_eq!(
        error.to_string(),
        "TCP box listeners are not available in this build"
    );
    assert!(!home.try_exists().unwrap());
}
