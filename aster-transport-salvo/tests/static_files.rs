//! Filesystem static-file serving (no iroh-blobs): `static_router` serves files
//! from a directory under a path prefix.

use std::fs;

use salvo::http::StatusCode as HttpStatus;
use salvo::prelude::*;
use salvo::test::{ResponseExt, TestClient};

#[tokio::test]
async fn serves_files_from_dir() {
    // A temp dir with one file.
    let dir = std::env::temp_dir().join(format!("aster-static-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("hello.txt"), b"hi there").unwrap();

    let service = Service::new(aster_transport_salvo::static_router("assets", &dir));

    let mut res = TestClient::get("http://localhost/assets/hello.txt")
        .send(&service)
        .await;
    assert_eq!(res.status_code, Some(HttpStatus::OK));
    assert_eq!(res.take_string().await.unwrap(), "hi there");

    // A missing file is a 404.
    let res = TestClient::get("http://localhost/assets/nope.txt")
        .send(&service)
        .await;
    assert_eq!(res.status_code, Some(HttpStatus::NOT_FOUND));

    let _ = fs::remove_dir_all(&dir);
}
