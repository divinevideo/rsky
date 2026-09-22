use super::*;

#[test]
fn configured_domains_match_whole_labels_with_or_without_a_leading_dot() {
    for domain in ["pds.example.com", ".pds.example.com"] {
        let domains = vec![domain.to_owned()];
        assert!(is_service_domain("alice.pds.example.com", &domains));
        assert!(
            ensure_handle_service_constraints("alice.pds.example.com", &domains, false).is_ok()
        );
        assert!(!is_service_domain("alice.evilpds.example.com", &domains));
        assert!(!is_service_domain("pds.example.com", &domains));
        assert!(
            ensure_handle_service_constraints("alice.deep.pds.example.com", &domains, false)
                .is_err()
        );
    }
}
