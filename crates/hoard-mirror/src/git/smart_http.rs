//! Read-only git smart HTTP. `gix` cannot serve `upload-pack`, so the protocol side is
//! delegated to the `git` binary and this module only frames and streams its output.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::Stream;
use hoard_index::config::SourceKind;
use tokio::sync::mpsc;

use crate::error::{MirrorError, Result};

/// `content-type` of the `info/refs?service=git-upload-pack` body.
pub const ADVERTISEMENT_CONTENT_TYPE: &str = "application/x-git-upload-pack-advertisement";
/// `content-type` of the `git-upload-pack` response body.
pub const RESULT_CONTENT_TYPE: &str = "application/x-git-upload-pack-result";
/// `cache-control` both git responses carry.
pub const CACHE_CONTROL: &str = "no-cache";
/// The only service hoard advertises.
pub const UPLOAD_PACK_SERVICE: &str = "git-upload-pack";
/// The service hoard refuses.
pub const RECEIVE_PACK_SERVICE: &str = "git-receive-pack";
/// Message returned for every push attempt.
pub const PUSH_REFUSED: &str = "this mirror is read-only: git-receive-pack is not implemented";

const GIT_BINARY: &str = "git";
const FLUSH_PACKET: &[u8] = b"0000";
const MAX_PACKET_PAYLOAD: usize = 65516;
const CHUNK_BYTES: usize = 64 * 1024;
const UPLOAD_PACK_TIMEOUT: Duration = Duration::from_secs(1800);
const STREAM_QUEUE_DEPTH: usize = 4;

/// The clone directory a git face request is served from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadPack {
    git_dir: PathBuf,
}

impl UploadPack {
    pub fn new(git_dir: impl Into<PathBuf>) -> Self {
        Self {
            git_dir: git_dir.into(),
        }
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Body of `GET /{org}/{repo}/info/refs?service=git-upload-pack`.
    pub async fn advertise(&self) -> Result<Vec<u8>> {
        advertise_refs(&self.git_dir).await
    }

    /// Body of `POST /{org}/{repo}/git-upload-pack`, streamed as git produces it.
    pub async fn fetch(
        &self,
        request_body: Bytes,
    ) -> Result<impl Stream<Item = Result<Bytes>> + Send + 'static> {
        upload_pack(&self.git_dir, request_body).await
    }
}

/// Frames a payload as a pkt-line: four lowercase hex digits of the total length,
/// the four length bytes included, followed by the payload.
pub fn pkt_line(payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() > MAX_PACKET_PAYLOAD {
        return Err(MirrorError::Git(format!(
            "pkt-line payload of {} bytes exceeds the {MAX_PACKET_PAYLOAD} byte limit",
            payload.len()
        )));
    }
    let length = payload.len() + FLUSH_PACKET.len();
    let mut framed = Vec::with_capacity(length);
    framed.extend_from_slice(format!("{length:04x}").as_bytes());
    framed.extend_from_slice(payload);
    Ok(framed)
}

/// The `# service=<name>` pkt-line followed by a flush packet, which prefixes every smart
/// HTTP ref advertisement.
pub fn service_header(service: &str) -> Result<Vec<u8>> {
    let mut header = pkt_line(format!("# service={service}\n").as_bytes())?;
    header.extend_from_slice(FLUSH_PACKET);
    Ok(header)
}

/// Runs `git upload-pack --advertise-refs` and prefixes the smart HTTP service header.
pub async fn advertise_refs(git_dir: &Path) -> Result<Vec<u8>> {
    let git_dir = git_dir.to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(GIT_BINARY)
            .arg("upload-pack")
            .arg("--stateless-rpc")
            .arg("--advertise-refs")
            .arg(&git_dir)
            .stdin(Stdio::null())
            .output()
    })
    .await
    .map_err(|error| MirrorError::Git(format!("upload-pack task failed: {error}")))??;
    if !output.status.success() {
        return Err(MirrorError::Git(format!(
            "upload-pack --advertise-refs exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut body = service_header(UPLOAD_PACK_SERVICE)?;
    body.extend_from_slice(&output.stdout);
    Ok(body)
}

/// Runs `git upload-pack --stateless-rpc`, feeding it the request body and streaming its
/// packfile back. The child is killed when the stream is dropped or the timeout expires.
pub async fn upload_pack(
    git_dir: &Path,
    request_body: Bytes,
) -> Result<impl Stream<Item = Result<Bytes>> + Send + 'static> {
    let git_dir = git_dir.to_path_buf();
    let mut child = tokio::task::spawn_blocking(move || {
        Command::new(GIT_BINARY)
            .arg("upload-pack")
            .arg("--stateless-rpc")
            .arg(&git_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    })
    .await
    .map_err(|error| MirrorError::Git(format!("upload-pack task failed: {error}")))??;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| MirrorError::Git("upload-pack stdin unavailable".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| MirrorError::Git("upload-pack stdout unavailable".to_owned()))?;

    tokio::task::spawn_blocking(move || {
        let _ = stdin.write_all(&request_body).and_then(|()| stdin.flush());
    });

    let shared = Arc::new(Mutex::new(Some(child)));
    let watchdog = Arc::clone(&shared);
    let handle = ChildHandle(shared);
    let (finished, deadline) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        if tokio::time::timeout(UPLOAD_PACK_TIMEOUT, deadline)
            .await
            .is_err()
        {
            kill_shared(&watchdog);
        }
    });

    let (sender, receiver) = mpsc::channel(STREAM_QUEUE_DEPTH);
    tokio::task::spawn_blocking(move || {
        pump(handle, stdout, &sender);
        drop(finished);
    });
    Ok(futures::stream::unfold(
        receiver,
        |mut receiver| async move { receiver.recv().await.map(|chunk| (chunk, receiver)) },
    ))
}

/// Only git sources can be cloned; cache-backed and loose repos stay on the Hub face and
/// answer 404 here.
pub const fn is_clonable(kind: SourceKind) -> bool {
    matches!(kind, SourceKind::Git)
}

/// The single answer every push route gives.
pub fn refuse_push() -> MirrorError {
    MirrorError::Git(PUSH_REFUSED.to_owned())
}

#[derive(Debug)]
struct ChildHandle(Arc<Mutex<Option<Child>>>);

impl Drop for ChildHandle {
    fn drop(&mut self) {
        kill_shared(&self.0);
        if let Some(mut child) = self.take() {
            let _ = child.wait();
        }
    }
}

impl ChildHandle {
    fn take(&self) -> Option<Child> {
        self.0.lock().ok().and_then(|mut slot| slot.take())
    }
}

fn kill_shared(shared: &Mutex<Option<Child>>) {
    if let Ok(mut slot) = shared.lock()
        && let Some(child) = slot.as_mut()
    {
        let _ = child.kill();
    }
}

fn pump(handle: ChildHandle, mut stdout: ChildStdout, sender: &mpsc::Sender<Result<Bytes>>) {
    let deadline = Instant::now() + UPLOAD_PACK_TIMEOUT;
    let mut buffer = vec![0u8; CHUNK_BYTES];
    loop {
        if Instant::now() > deadline {
            let _ = sender.blocking_send(Err(MirrorError::Git(
                "upload-pack exceeded its time limit".to_owned(),
            )));
            return;
        }
        match stdout.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if sender
                    .blocking_send(Ok(Bytes::copy_from_slice(&buffer[..read])))
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                let _ = sender.blocking_send(Err(MirrorError::Io(error)));
                return;
            }
        }
    }
    let Some(mut child) = handle.take() else {
        return;
    };
    match child.wait() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            let _ = sender.blocking_send(Err(MirrorError::Git(format!(
                "upload-pack exited with {status}"
            ))));
        }
        Err(error) => {
            let _ = sender.blocking_send(Err(MirrorError::Io(error)));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use futures::StreamExt;
    use tempfile::TempDir;

    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("git output is utf-8")
    }

    fn sample_repo() -> TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path();
        git(path, &["init", "--initial-branch=main", "--quiet"]);
        git(path, &["config", "user.email", "hoard@example.invalid"]);
        git(path, &["config", "user.name", "Hoard Test"]);
        fs::write(path.join("README.md"), "# sample\n").expect("file written");
        git(path, &["add", "."]);
        git(path, &["commit", "--quiet", "-m", "first commit"]);
        git(path, &["tag", "v1.0"]);
        dir
    }

    fn parse_packets(body: &[u8]) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        let mut cursor = 0usize;
        while cursor + 4 <= body.len() {
            let header = std::str::from_utf8(&body[cursor..cursor + 4]).expect("hex header");
            let length = usize::from_str_radix(header, 16).expect("hex length");
            if length == 0 {
                cursor += 4;
                continue;
            }
            packets.push(body[cursor + 4..cursor + length].to_vec());
            cursor += length;
        }
        packets
    }

    #[test]
    fn pkt_lines_carry_their_own_length() {
        assert_eq!(pkt_line(b"a\n").expect("framed"), b"0006a\n");
        assert_eq!(pkt_line(b"").expect("framed"), b"0004");
        let header = service_header(UPLOAD_PACK_SERVICE).expect("header");
        assert_eq!(header, b"001e# service=git-upload-pack\n0000");
        assert_eq!(
            std::str::from_utf8(&header[..4]).expect("hex"),
            format!("{:04x}", header.len() - FLUSH_PACKET.len())
        );
        assert!(pkt_line(&vec![0u8; MAX_PACKET_PAYLOAD + 1]).is_err());
    }

    #[test]
    fn pkt_line_framing_round_trips() {
        let payloads: [&[u8]; 3] = [b"# service=git-upload-pack\n", b"want 0000\n", b"x"];
        let mut body = Vec::new();
        for payload in payloads {
            body.extend_from_slice(&pkt_line(payload).expect("framed"));
        }
        body.extend_from_slice(FLUSH_PACKET);
        assert_eq!(parse_packets(&body), payloads.map(<[u8]>::to_vec));
    }

    #[tokio::test]
    async fn advertisement_lists_the_same_refs_as_show_ref() {
        let dir = sample_repo();
        let body = advertise_refs(&dir.path().join(".git"))
            .await
            .expect("advertisement");
        assert!(body.starts_with(b"001e# service=git-upload-pack\n0000"));

        let advertised: Vec<String> = parse_packets(&body)
            .into_iter()
            .filter_map(|packet| {
                let text = String::from_utf8(packet).expect("packet is utf-8");
                let text = text
                    .split('\0')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let (id, name) = text.split_once(' ')?;
                name.starts_with("refs/").then(|| format!("{id} {name}"))
            })
            .collect();
        let expected: Vec<String> = git(dir.path(), &["show-ref", "-d"])
            .lines()
            .map(str::to_owned)
            .collect();
        assert!(!expected.is_empty());
        assert_eq!(advertised, expected);
    }

    #[tokio::test]
    async fn upload_pack_streams_a_packfile_for_a_want() {
        let dir = sample_repo();
        let head = git(dir.path(), &["rev-parse", "HEAD"]).trim().to_owned();
        let mut request =
            pkt_line(format!("want {head} multi_ack_detailed side-band-64k\n").as_bytes())
                .expect("want framed");
        request.extend_from_slice(FLUSH_PACKET);
        request.extend_from_slice(&pkt_line(b"done\n").expect("done framed"));

        let stream = upload_pack(&dir.path().join(".git"), Bytes::from(request))
            .await
            .expect("upload-pack starts");
        futures::pin_mut!(stream);
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            body.extend_from_slice(&chunk.expect("chunk"));
        }
        assert!(!body.is_empty());
        assert!(body.windows(4).any(|window| window == b"PACK"));
    }

    #[tokio::test]
    async fn advertise_refs_reports_a_missing_repository() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(advertise_refs(&dir.path().join("absent")).await.is_err());
    }

    #[test]
    fn only_git_sources_are_clonable() {
        assert!(is_clonable(SourceKind::Git));
        assert!(!is_clonable(SourceKind::HfCache));
        assert!(!is_clonable(SourceKind::Loose));
        assert!(!is_clonable(SourceKind::Auto));
        assert!(matches!(refuse_push(), MirrorError::Git(message) if message == PUSH_REFUSED));
    }
}
