//! Read-only admission for pre-panel-2 return journals. Never execute the old
//! dual-public-name protocol; retain it as evidence for doctor/manual recovery.
use super::{Store, discovery, guard};
use anyhow::{Context, Result, ensure};
use std::fs;
pub(super) fn ensure_not_pending(store: &Store, id: &str) -> Result<()> {
    ensure!(discovery::valid_id(id), "대화 ID는 UUID여야 합니다");
    let p = store
        .home
        .join("journal")
        .join(format!("sessions-return-{id}.json"));
    guard::safe_path(&store.home, &p)?;
    if !p.try_exists()? {
        return Ok(());
    }
    ensure!(
        fs::metadata(&p)?.len() < 64 * 1024 * 1024,
        "돌려보내기 기록이 너무 큽니다"
    );
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&p)?)?;
    ensure!(value["schema"] == 1, "지원하지 않는 돌려보내기 기록입니다");
    let record: serde_json::Value = serde_json::from_slice(&crate::cutover::unprotect(
        value["payload"]
            .as_str()
            .context("돌려보내기 보호 기록이 없습니다")?,
    )?)?;
    ensure!(
        record["id"] == id && record["phase"] == "Committed",
        "이전 버전의 미완료 돌려보내기가 있습니다. doctor로 확인하세요"
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_pending_return_is_never_executed_and_preserves_evidence() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/r9-legacy-return");
        fs::create_dir_all(&base).unwrap();
        let temp = tempfile::tempdir_in(fs::canonicalize(base).unwrap()).unwrap();
        fs::create_dir_all(temp.path().join("journal")).unwrap();
        let store = Store {
            home: temp.path().to_owned(),
            target: temp.path().to_owned(),
            search_homes: Vec::new(),
            map_path: temp.path().join("map.json"),
        };
        let id = "11111111-1111-1111-1111-111111111111";
        let path = store
            .home
            .join("journal")
            .join(format!("sessions-return-{id}.json"));
        assert!(ensure_not_pending(&store, id).is_ok());
        for (phase, allowed) in [("Prepared", false), ("Mapped", false), ("Committed", true)] {
            let payload = crate::cutover::protect(
                &serde_json::to_vec(&serde_json::json!({"id":id,"phase":phase})).unwrap(),
            )
            .unwrap();
            let bytes =
                serde_json::to_vec(&serde_json::json!({"schema":1,"payload":payload})).unwrap();
            fs::write(&path, &bytes).unwrap();
            assert_eq!(ensure_not_pending(&store, id).is_ok(), allowed);
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }
}
