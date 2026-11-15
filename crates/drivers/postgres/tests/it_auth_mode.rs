//! A profile set to "no authentication" never sends the password (only with `DBSTUDIO_IT=1`).

mod common;

use common::*;
use dbstudio_core::{AuthMode, Error, TlsMode};

#[tokio::test]
async fn no_auth_mode_ignores_the_secret_and_fails_against_a_password_server() {
    if !enabled() {
        return;
    }
    let mut cfg = config(PG16, TlsMode::Prefer);
    // The correct password is supplied, yet the driver must not use it.
    assert!(try_connect(&cfg, PASSWORD).await.is_ok());
    cfg.auth_mode = AuthMode::NoAuth;
    let err = try_connect(&cfg, PASSWORD)
        .await
        .err()
        .expect("password server must refuse a login without password");
    assert!(matches!(err, Error::Authentication(_)), "{err:?}");
}
