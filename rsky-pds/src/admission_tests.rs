use super::*;

const FILE: &str = r#"
version = 1
default = "absent"

[entries]
"did:plc:active" = "active"
"did:plc:draining" = "draining"
"did:plc:repair" = { state = "maintenance", workflow_id = "repair-7" }
"#;

#[test]
fn parses_every_state_and_the_default() {
    let list = Allowlist::parse(FILE).unwrap();
    assert_eq!(list.default, AdmissionState::Absent);
    assert_eq!(list.state_of("did:plc:active"), &AdmissionState::Active);
    assert_eq!(list.state_of("did:plc:draining"), &AdmissionState::Draining);
    assert_eq!(
        list.state_of("did:plc:repair"),
        &AdmissionState::Maintenance {
            workflow_id: "repair-7".to_owned()
        }
    );
    assert_eq!(list.state_of("did:plc:other"), &AdmissionState::Absent);
    assert_eq!(
        list.state_of("did:plc:repair").workflow_id(),
        Some("repair-7")
    );
    assert_eq!(AdmissionState::Draining.name(), "draining");
    assert_eq!(AdmissionState::Active.workflow_id(), None);

    let open = Allowlist::parse("version = 1\ndefault = \"active\"\n").unwrap();
    assert_eq!(open.state_of("did:plc:anyone"), &AdmissionState::Active);
    let implicit = Allowlist::parse("version = 1\n").unwrap();
    assert_eq!(implicit.default, AdmissionState::Absent);
    assert_eq!(Allowlist::unrestricted().default, AdmissionState::Active);
}

#[test]
fn rejects_malformed_files() {
    for (text, needle) in [
            ("not toml [", "valid TOML"),
            ("version = 2\n", "version"),
            ("version = 1\ndefault = \"whatever\"\n", "unknown admission state"),
            (
                "version = 1\n[entries]\n\"did:plc:a\" = \"maintenance\"\n",
                "workflow_id",
            ),
            (
                "version = 1\n[entries]\n\"did:plc:a\" = { state = \"active\", workflow_id = \"x\" }\n",
                "only a maintenance entry",
            ),
            (
                "version = 1\n[entries]\n\"did:plc:a\" = { state = \"maintenance\", workflow_id = \"\" }\n",
                "non-empty",
            ),
            ("version = 1\n[entries]\nalice = \"active\"\n", "not a DID"),
        ] {
            let err = Allowlist::parse(text).unwrap_err();
            assert!(
                format!("{err:#}").contains(needle),
                "{text}: {err:#} lacks {needle}"
            );
        }
}

#[test]
fn admission_decisions_follow_the_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write-allowlist.toml");
    std::fs::write(&path, FILE).unwrap();
    let admission = Admission::from_file(&path).unwrap();
    assert_eq!(admission.source(), Some(path.as_path()));

    assert!(admission.admit_mutation("did:plc:active").is_ok());
    assert!(admission.admit_worker("did:plc:active").is_ok());
    assert_eq!(
        admission.admit_mutation("did:plc:draining"),
        Err(NotAdmitted {
            did: "did:plc:draining".to_owned(),
            state: "draining".to_owned()
        })
    );
    assert!(admission.admit_worker("did:plc:draining").is_ok());
    assert!(admission.admit_mutation("did:plc:repair").is_err());
    assert!(admission.admit_worker("did:plc:repair").is_ok());
    assert!(admission
        .admit_maintenance("did:plc:repair", "repair-7")
        .is_ok());
    assert!(admission
        .admit_maintenance("did:plc:repair", "repair-8")
        .is_err());
    assert!(admission
        .admit_maintenance("did:plc:active", "repair-7")
        .is_err());
    let absent = admission.admit_worker("did:plc:other").unwrap_err();
    assert_eq!(absent.state, "absent");
    assert!(absent.to_string().contains("not admitted"));
    assert_eq!(admission.state_of("did:plc:other"), AdmissionState::Absent);

    let open = Admission::unrestricted();
    assert!(open.admit_mutation("did:plc:anyone").is_ok());
    assert!(!open.reload().unwrap());
    assert!(open.source().is_none());
}

#[test]
fn reload_installs_changes_and_keeps_the_last_good_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write-allowlist.toml");
    std::fs::write(&path, FILE).unwrap();
    let admission = Admission::from_file(&path).unwrap();
    assert!(
        !admission.reload().unwrap(),
        "unchanged file is not reloaded"
    );

    // a change to the file is picked up
    std::thread::sleep(Duration::from_millis(20));
    let later = std::time::SystemTime::now() + Duration::from_secs(5);
    std::fs::write(&path, "version = 1\ndefault = \"active\"\n").unwrap();
    std::fs::File::open(&path)
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert!(admission.reload().unwrap());
    assert!(admission.admit_mutation("did:plc:other").is_ok());

    // a broken file is rejected and the previous allowlist stays
    std::fs::write(&path, "version = 3\n").unwrap();
    std::fs::File::open(&path)
        .unwrap()
        .set_modified(later + Duration::from_secs(5))
        .unwrap();
    assert!(admission.reload().is_err());
    assert!(admission.admit_mutation("did:plc:other").is_ok());

    // a missing file cannot be loaded at all
    assert!(Admission::from_file(dir.path().join("missing.toml")).is_err());
    std::fs::remove_file(&path).unwrap();
    assert!(admission.reload().is_err());
}

#[tokio::test]
async fn reloader_polls_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write-allowlist.toml");
    std::fs::write(&path, FILE).unwrap();
    let admission = Arc::new(Admission::from_file(&path).unwrap());
    admission.spawn_reloader(Duration::from_millis(10));
    Arc::new(Admission::unrestricted()).spawn_reloader(Duration::from_millis(10));
    let later = std::time::SystemTime::now() + Duration::from_secs(5);
    std::fs::write(&path, "version = 1\ndefault = \"active\"\n").unwrap();
    std::fs::File::open(&path)
        .unwrap()
        .set_modified(later)
        .unwrap();
    for _ in 0..100 {
        if admission.admit_mutation("did:plc:other").is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(admission.admit_mutation("did:plc:other").is_ok());
    // a broken file is logged and the previous allowlist stays
    std::fs::write(&path, "version = 3\n").unwrap();
    std::fs::File::open(&path)
        .unwrap()
        .set_modified(later + Duration::from_secs(5))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(admission.admit_mutation("did:plc:other").is_ok());
}

/// A fleet rollback drains every actor at once: the default refuses
/// new mutations while workers finish, and an explicit entry still
/// overrides it.
#[test]
fn a_draining_default_refuses_mutations_and_keeps_workers_running() {
    let list = Allowlist::parse(
        "version = 1\ndefault = \"draining\"\n[entries]\n\"did:plc:a\" = \"active\"\n",
    )
    .unwrap();
    assert_eq!(list.default, AdmissionState::Draining);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("allowlist.toml");
    std::fs::write(
        &path,
        "version = 1\ndefault = \"draining\"\n[entries]\n\"did:plc:a\" = \"active\"\n",
    )
    .unwrap();
    let admission = Admission::from_file(&path).unwrap();
    assert_eq!(
        admission
            .admit_mutation("did:plc:anyone")
            .unwrap_err()
            .state,
        "draining"
    );
    assert!(admission.admit_worker("did:plc:anyone").is_ok());
    assert!(admission.admit_mutation("did:plc:a").is_ok());
    assert_eq!(admission.state_of("did:plc:zzz"), AdmissionState::Draining);
}
