use super::*;

#[test]
fn policy_create_replace_and_load_have_one_protected_owner_ace() {
    let home = home();
    let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
    for n in 0..2 {
        store
            .put_project(
                None,
                Project {
                    name: format!("fixture-{n}"),
                    settings: Settings::default(),
                },
            )
            .unwrap();
        let saved = home.path().join("acl.txt");
        let output = std::process::Command::new("icacls.exe")
            .arg(&store.path)
            .arg("/save")
            .arg(&saved)
            .arg("/q")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "synthetic policy ACL inspection failed"
        );
        let bytes = fs::read(&saved).unwrap();
        let text = if bytes.starts_with(&[255, 254]) || bytes.contains(&0) {
            let offset = usize::from(bytes.starts_with(&[255, 254])) * 2;
            String::from_utf16(
                &bytes[offset..]
                    .chunks_exact(2)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
                    .collect::<Vec<_>>(),
            )
            .unwrap()
        } else {
            String::from_utf8(bytes).unwrap()
        };
        assert!(text.contains("D:P"), "policy ACL must be protected");
        assert_eq!(
            text.matches("(A;").count(),
            1,
            "only the current owner may access intent"
        );
        assert!(PolicyStore::new(home.path(), Settings::default()).is_ok());
    }
}
