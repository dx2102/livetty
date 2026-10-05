//! Resumable uploads for big files: the subset of tus 1.0.0 (core, creation,
//! termination) that tus-js-client needs. Small files keep using the plain
//! one-shot /api/file/upload.
//!
//! The partial file is "<dest>.part", next to the destination and not hidden.
//! Its tus state lives in extended attributes, so there is no side file: the
//! upload id and the total length declared at creation. The current offset is
//! simply the size of the .part file, since chunks are only appended in order.
//!
//! The upload URL is /api/tus/<id>/<base64url of dest>, so resuming needs no
//! table on the server and still works after a restart.
//!
//! On the last byte the .part is hard-linked to the destination and removed.
//! Unlike rename(), link() fails when the destination exists, so an existing
//! file is never overwritten.

use crate::files::{err, require_abs};
use axum::{
    body::Body,
    extract::Path as UrlPath,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use futures_util::StreamExt;
use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{ErrorKind, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

const XATTR_ID: &str = "user.livetty.upload_id";
const XATTR_LEN: &str = "user.livetty.upload_length";

fn part_of(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

/// Upload-Metadata is "key base64,key base64,...".
fn metadata(headers: &HeaderMap, key: &str) -> Option<String> {
    let raw = headers.get("upload-metadata")?.to_str().ok()?;
    raw.split(',').find_map(|kv| {
        let mut it = kv.trim().splitn(2, ' ');
        if it.next()? != key {
            return None;
        }
        String::from_utf8(STANDARD.decode(it.next()?).ok()?).ok()
    })
}

fn tus_response(status: StatusCode, headers: &[(&'static str, String)]) -> Response {
    let mut r = status.into_response();
    let h = r.headers_mut();
    h.insert("tus-resumable", HeaderValue::from_static("1.0.0"));
    for (k, v) in headers {
        if let Ok(v) = HeaderValue::from_str(v) {
            h.insert(*k, v);
        }
    }
    r
}

/// Resolve an upload URL to (dest, part, total length). The id must match the
/// one stored on the .part, so a stale URL never touches a newer upload.
fn find_upload(id: &str, enc: &str) -> Result<(PathBuf, PathBuf, u64), Response> {
    let not_found = || err(StatusCode::NOT_FOUND, "upload not found");
    let bytes = URL_SAFE_NO_PAD.decode(enc).map_err(|_| not_found())?;
    let dest = PathBuf::from(OsStr::from_bytes(&bytes));
    if !dest.is_absolute() {
        return Err(not_found());
    }
    let part = part_of(&dest);
    match xattr::get(&part, XATTR_ID) {
        Ok(Some(v)) if v == id.as_bytes() => {}
        _ => return Err(not_found()),
    }
    let total = xattr::get(&part, XATTR_LEN)
        .ok()
        .flatten()
        .and_then(|v| String::from_utf8(v).ok())
        .and_then(|s| s.parse().ok())
        .ok_or_else(not_found)?;
    Ok((dest, part, total))
}

/// POST /api/tus
pub async fn create(headers: HeaderMap) -> Response {
    let Some(total) = header_u64(&headers, "upload-length") else {
        return err(StatusCode::BAD_REQUEST, "missing Upload-Length");
    };
    let Some(path) = metadata(&headers, "path") else {
        return err(StatusCode::BAD_REQUEST, "missing path in Upload-Metadata");
    };
    let dest = match require_abs(&path) {
        Ok(p) => p,
        Err(r) => return r,
    };
    // Checked up front so a big upload does not run for nothing.
    if dest.exists() {
        return err(StatusCode::CONFLICT, format!("file already exists: {}", dest.display()));
    }
    if !dest.parent().is_some_and(|d| d.is_dir()) {
        return err(StatusCode::NOT_FOUND, format!("target directory does not exist: {}", dest.display()));
    }
    let part = part_of(&dest);
    let file = match OpenOptions::new().write(true).create_new(true).open(&part) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            // A leftover from an earlier upload to the same place is ours to
            // start over. A .part we did not create is not ours to touch.
            if !matches!(xattr::get(&part, XATTR_ID), Ok(Some(_))) {
                return err(
                    StatusCode::CONFLICT,
                    format!("{} exists and was not created by livetty", part.display()),
                );
            }
            match OpenOptions::new().write(true).open(&part) {
                Ok(f) => f,
                Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot open {}: {e}", part.display())),
            }
        }
        Err(e) => {
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot create {}: {e}", part.display()))
        }
    };
    if file.try_lock().is_err() {
        return err(StatusCode::LOCKED, "another upload to this file is in progress");
    }
    let id = format!("{:016x}", rand::random::<u64>());
    let setup = file
        .set_len(0)
        .and_then(|_| xattr::set(&part, XATTR_ID, id.as_bytes()))
        .and_then(|_| xattr::set(&part, XATTR_LEN, total.to_string().as_bytes()));
    if let Err(e) = setup {
        let _ = std::fs::remove_file(&part);
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot set up {}: {e}", part.display()));
    }
    let url = format!("/api/tus/{id}/{}", URL_SAFE_NO_PAD.encode(dest.as_os_str().as_bytes()));
    tus_response(StatusCode::CREATED, &[("location", url)])
}

/// HEAD /api/tus/{id}/{dest}
pub async fn head(UrlPath((id, enc)): UrlPath<(String, String)>) -> Response {
    let (_, part, total) = match find_upload(&id, &enc) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Ok(meta) = std::fs::metadata(&part) else {
        return err(StatusCode::NOT_FOUND, "upload not found");
    };
    tus_response(
        StatusCode::OK,
        &[
            ("upload-offset", meta.len().to_string()),
            ("upload-length", total.to_string()),
            ("cache-control", "no-store".to_string()),
        ],
    )
}

/// PATCH /api/tus/{id}/{dest}
/// The body is streamed to disk as it arrives. If the client goes away mid
/// chunk, whatever was written stays and HEAD reports it as the new offset.
pub async fn patch(
    UrlPath((id, enc)): UrlPath<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok());
    if ct != Some("application/offset+octet-stream") {
        return err(StatusCode::UNSUPPORTED_MEDIA_TYPE, "expected application/offset+octet-stream");
    }
    let Some(offset) = header_u64(&headers, "upload-offset") else {
        return err(StatusCode::BAD_REQUEST, "missing Upload-Offset");
    };
    let (dest, part, total) = match find_upload(&id, &enc) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let file = match OpenOptions::new().write(true).open(&part) {
        Ok(f) => f,
        Err(_) => return err(StatusCode::NOT_FOUND, "upload not found"),
    };
    // Held until this request ends, including the final link below.
    if file.try_lock().is_err() {
        return err(StatusCode::LOCKED, "another request is writing this upload");
    }
    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if offset != len {
        return err(StatusCode::CONFLICT, format!("offset mismatch, server has {len} bytes"));
    }
    let mut file = tokio::fs::File::from_std(file);
    if let Err(e) = file.seek(SeekFrom::Start(len)).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    let mut written = len;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        if written + chunk.len() as u64 > total {
            let _ = file.flush().await;
            return err(StatusCode::BAD_REQUEST, "more data than Upload-Length");
        }
        if let Err(e) = file.write_all(&chunk).await {
            let _ = file.flush().await;
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("write failed: {e}"));
        }
        written += chunk.len() as u64;
    }
    if let Err(e) = file.flush().await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("write failed: {e}"));
    }
    if written == total {
        if let Err(e) = std::fs::hard_link(&part, &dest) {
            if e.kind() != ErrorKind::AlreadyExists {
                return err(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot create {}: {e}", dest.display()));
            }
            // Someone created the destination during the upload. Drop the
            // .part so the client's retry starts over and reports the clash.
            let _ = std::fs::remove_file(&part);
            return err(StatusCode::CONFLICT, format!("file already exists: {}", dest.display()));
        }
        // dest shares the inode, so this also clears them from the result.
        let _ = xattr::remove(&dest, XATTR_ID);
        let _ = xattr::remove(&dest, XATTR_LEN);
        let _ = std::fs::remove_file(&part);
    }
    drop(file);
    tus_response(StatusCode::NO_CONTENT, &[("upload-offset", written.to_string())])
}

/// DELETE /api/tus/{id}/{dest}
pub async fn delete(UrlPath((id, enc)): UrlPath<(String, String)>) -> Response {
    let (_, part, _) = match find_upload(&id, &enc) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(e) = std::fs::remove_file(&part) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    tus_response(StatusCode::NO_CONTENT, &[])
}
