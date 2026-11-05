use serde_json::json;
use spotify_library_sync::{
    auth::challenge,
    spotify::{quota_exceeded, retry_after, Page},
};
#[test]
fn pkce_matches_rfc7636_vector() {
    assert_eq!(
        challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}
#[test]
fn rate_limit_protocol_and_quota_detection() {
    assert_eq!(retry_after(Some("123")).as_secs(), 123);
    assert_eq!(retry_after(None).as_secs(), 60);
    assert!(quota_exceeded(
        &json!({"error":{"reason":"QUOTA_EXCEEDED"}})
    ));
    assert!(!quota_exceeded(&json!({"error":{"message":"Forbidden"}})));
    let future = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
    assert!((118..=120).contains(&retry_after(Some(&httpdate::fmt_http_date(future))).as_secs()));
}
#[test]
fn incomplete_pages_cannot_finalize() {
    let page = Page {
        items: vec![],
        offset: 0,
        total: 2,
        next: None,
    };
    assert!(page.validate(0, None).is_err());
    let page = Page {
        items: vec![json!({})],
        offset: 0,
        total: 1,
        next: None,
    };
    assert!(page.validate(0, Some(2)).is_err());
    assert!(page.validate(2, None).is_err());
    assert!(page.validate(0, Some(1)).unwrap());
}
