use axum::{
    body::Bytes,
    extract::Query,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const READ_MAX: u64 = 2 * 1024 * 1024; // 2 MiB
/// Cap raw download response size. Larger files fall through into 413.
const DOWNLOAD_MAX: u64 = 500 * 1024 * 1024; // 500 MiB
/// Upload body cap (per-route DefaultBodyLimit set on the route as well).
pub const UPLOAD_MAX: usize = 500 * 1024 * 1024; // 500 MiB

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({"error": msg.into()}))).into_response()
}

fn require_abs(p: &str) -> Result<PathBuf, Response> {
    let path = PathBuf::from(p);
    if !path.is_absolute() {
        return Err(err(StatusCode::BAD_REQUEST, "path must be absolute"));
    }
    Ok(path)
}

fn mtime_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(serde::Deserialize)]
pub struct PathQ {
    path: String,
}

pub async fn list_dir(Query(q): Query<PathQ>) -> Response {
    let path = match require_abs(&q.path) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let rd = match std::fs::read_dir(&path) {
        Ok(rd) => rd,
        Err(e) => return err(StatusCode::NOT_FOUND, format!("{e}")),
    };
    let mut entries: Vec<serde_json::Value> = vec![];
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        let meta = ent.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        entries.push(json!({
            "name": name,
            "is_dir": is_dir,
            "size": meta.as_ref().map(|m| m.len()).unwrap_or(0),
            "mtime_ms": meta.as_ref().map(mtime_ms).unwrap_or(0),
        }));
    }
    entries.sort_by(|a, b| {
        let (ad, bd) = (a["is_dir"].as_bool().unwrap(), b["is_dir"].as_bool().unwrap());
        bd.cmp(&ad)
            .then_with(|| a["name"].as_str().unwrap().cmp(b["name"].as_str().unwrap()))
    });
    Json(json!({"path": path, "entries": entries})).into_response()
}

pub async fn read_file(Query(q): Query<PathQ>) -> Response {
    let path = match require_abs(&q.path) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => return err(StatusCode::NOT_FOUND, format!("{e}")),
    };
    if !meta.is_file() {
        return err(StatusCode::BAD_REQUEST, "not a regular file");
    }
    if meta.len() > READ_MAX {
        return err(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("file too large ({} bytes > {READ_MAX}), open it in the terminal instead", meta.len()),
        );
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")),
    };
    let head = &bytes[..bytes.len().min(8192)];
    if head.contains(&0) {
        return err(StatusCode::UNSUPPORTED_MEDIA_TYPE, "binary file, cannot edit");
    }
    let content = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return err(StatusCode::UNSUPPORTED_MEDIA_TYPE, "not valid UTF-8"),
    };
    Json(json!({"content": content, "mtime_ms": mtime_ms(&meta)})).into_response()
}

#[derive(serde::Deserialize)]
pub struct WriteReq {
    path: String,
    content: String,
    /// The mtime recorded when the file was opened; reject on mismatch (external modification conflict).
    expect_mtime_ms: Option<u64>,
}

pub async fn write_file(Json(body): Json<WriteReq>) -> Response {
    let path = match require_abs(&body.path) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if let (Some(expect), Ok(meta)) = (body.expect_mtime_ms, std::fs::metadata(&path)) {
        let cur = mtime_ms(&meta);
        if cur != expect {
            let disk = std::fs::read_to_string(&path).ok();
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "file was modified externally",
                    "mtime_ms": cur,
                    "disk_content": disk,
                })),
            )
                .into_response();
        }
    }
    // Atomic write: temp file in the same directory + rename
    let dir = path.parent().unwrap_or(Path::new("/"));
    let tmp = dir.join(format!(
        ".{}.remote-tmp-{}",
        path.file_name().map(|s| s.to_string_lossy()).unwrap_or_default(),
        std::process::id()
    ));
    if let Err(e) = std::fs::write(&tmp, &body.content) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("temp file write failed: {e}"));
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("rename failed: {e}"));
    }
    let mtime = std::fs::metadata(&path).map(|m| mtime_ms(&m)).unwrap_or(0);
    Json(json!({"ok": true, "mtime_ms": mtime})).into_response()
}

/// GET /api/file/download?path=/abs/path
/// Streams the file raw with `Content-Disposition: attachment` so the browser saves it.
pub async fn download(Query(q): Query<PathQ>) -> Response {
    let path = match require_abs(&q.path) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => return err(StatusCode::NOT_FOUND, format!("{e}")),
    };
    if !meta.is_file() {
        return err(StatusCode::BAD_REQUEST, "not a regular file");
    }
    if meta.len() > DOWNLOAD_MAX {
        return err(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("file too large ({} bytes > {DOWNLOAD_MAX})", meta.len()),
        );
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")),
    };
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".into());
    // RFC 6266: use filename* with UTF-8 encoding so non-ASCII names work.
    let disp = format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        name.replace('\\', "\\\\").replace('"', "\\\""),
        urlencode(&name),
    );
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_DISPOSITION, disp),
        ],
        bytes,
    )
        .into_response()
}

/// Minimal RFC 3986 percent-encoder for the download filename* header.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// POST /api/file/upload?path=/abs/dest/file
/// Writes raw request body to `path`. Refuses to overwrite existing files ,
/// caller must delete first if that's intended.
pub async fn upload(Query(q): Query<PathQ>, body: Bytes) -> Response {
    let path = match require_abs(&q.path) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if path.exists() {
        return err(
            StatusCode::CONFLICT,
            format!("file already exists: {}", path.display()),
        );
    }
    let dir = path.parent().unwrap_or(Path::new("/"));
    if !dir.exists() {
        return err(
            StatusCode::NOT_FOUND,
            format!("target directory does not exist: {}", dir.display()),
        );
    }
    // Atomic write via temp + rename
    let tmp = dir.join(format!(
        ".{}.remote-upload-{}",
        path.file_name().map(|s| s.to_string_lossy()).unwrap_or_default(),
        std::process::id()
    ));
    if let Err(e) = std::fs::write(&tmp, &body) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("write failed: {e}"));
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("rename failed: {e}"));
    }
    let mtime = std::fs::metadata(&path).map(|m| mtime_ms(&m)).unwrap_or(0);
    Json(json!({"ok": true, "size": body.len(), "mtime_ms": mtime})).into_response()
}

#[derive(serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FsOp {
    Mkdir { path: String },
    Rename { path: String, to: String },
    Delete { path: String },
    Touch { path: String },
}

pub async fn fs_op(Json(op): Json<FsOp>) -> Response {
    let result = match &op {
        FsOp::Mkdir { path } => match require_abs(path) {
            Ok(p) => std::fs::create_dir_all(p).map_err(|e| e.to_string()),
            Err(r) => return r,
        },
        FsOp::Rename { path, to } => match (require_abs(path), require_abs(to)) {
            (Ok(a), Ok(b)) => std::fs::rename(a, b).map_err(|e| e.to_string()),
            (Err(r), _) | (_, Err(r)) => return r,
        },
        FsOp::Delete { path } => match require_abs(path) {
            Ok(p) => {
                if p.is_dir() {
                    // Only delete empty directories, to guard against accidents
                    std::fs::remove_dir(p).map_err(|e| e.to_string())
                } else {
                    std::fs::remove_file(p).map_err(|e| e.to_string())
                }
            }
            Err(r) => return r,
        },
        FsOp::Touch { path } => match require_abs(path) {
            Ok(p) => {
                if p.exists() {
                    Err("file already exists".to_string())
                } else {
                    std::fs::write(p, "").map_err(|e| e.to_string())
                }
            }
            Err(r) => return r,
        },
    };
    match result {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}
