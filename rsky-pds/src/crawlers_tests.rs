use super::*;

#[test]
fn crawl_request_advertises_pds_hostname() {
    let crawlers = Crawlers::new(
        "pds.example.com".to_string(),
        vec![
            "https://relay1.example".to_string(),
            "https://relay2.example".to_string(),
        ],
    );
    assert_eq!(crawlers.crawl_request().hostname, "pds.example.com");
}
