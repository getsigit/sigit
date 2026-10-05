//! Reading and writing files through the ACP client instead of the disk.
//!
//! An editor that advertises `fs.readTextFile` / `fs.writeTextFile` in its
//! `initialize` request will serve `fs/read_text_file` and `fs/write_text_file`
//! from its own buffers. Going through it is what lets `read_file` see changes
//! the user has not saved yet, and lets `edit_file` land in the open buffer
//! rather than underneath it, where the editor would have to reconcile the
//! file on disk with a buffer the user may have modified.
//!
//! The tool code in `tools.rs` has no access to the connection, so this module
//! is the seam, the same shape as the one `mcp::call_tool` gives MCP: `main.rs`
//! registers a [`ClientFileSystem`] once the client has said what it supports,
//! and the file tools ask [`route_for`] whether a path should go through it.
//!
//! The disk stays the fallback everywhere. Nothing is registered in the TUI or
//! headless modes, a path outside the session's roots is not the editor's to
//! serve, and a request the client fails or never answers is retried on disk,
//! so a client with a partial or broken implementation costs a log line rather
//! than the tool call.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;

/// How long the client gets to answer one file request before the disk is used
/// instead. A buffer read or write is local work for an editor; a client that
/// takes longer than this has most likely dropped the request.
const CLIENT_FS_TIMEOUT: Duration = Duration::from_secs(30);

/// The client's side of ACP's file-system methods. Errors are plain strings:
/// they only ever reach the log, since the caller falls back to the disk.
#[async_trait]
pub trait ClientFileSystem: Send + Sync {
    async fn read_text_file(&self, session_id: &str, path: &Path) -> Result<String, String>;
    async fn write_text_file(
        &self,
        session_id: &str,
        path: &Path,
        content: &str,
    ) -> Result<(), String>;
}

#[derive(Clone)]
struct Registered {
    fs: Arc<dyn ClientFileSystem>,
    read: bool,
    write: bool,
}

/// The connected client's file system, with the two capabilities it
/// advertised. Capabilities are fixed for the connection, and the process
/// serves one connection, so this is set once from `initialize`.
static CLIENT: RwLock<Option<Registered>> = RwLock::new(None);

/// Record what the client offered in `clientCapabilities.fs`. A client that
/// offered neither method leaves the disk as the only path.
pub fn register(fs: Arc<dyn ClientFileSystem>, read: bool, write: bool) {
    let registered = (read || write).then_some(Registered { fs, read, write });
    if let Ok(mut guard) = CLIENT.write() {
        *guard = registered;
    }
}

/// `SIGIT_CLIENT_FS=off` keeps every file tool on the disk, for a client whose
/// file-system methods misbehave in a way the fallback cannot see.
pub fn disabled_by_env() -> bool {
    std::env::var("SIGIT_CLIENT_FS")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "off" | "0" | "false"
            )
        })
        .unwrap_or(false)
}

/// One file's way through the client, for the session that asked.
pub struct Route {
    fs: Arc<dyn ClientFileSystem>,
    session_id: String,
    read: bool,
    write: bool,
}

/// The client route for `path`, or `None` when the tool should use the disk:
/// no client file system is registered, no session is live, or the path is
/// outside the session's roots. `path` must be absolute, which is also what
/// the protocol requires of the paths it carries.
pub fn route_for(session_id: Option<&str>, path: &Path) -> Option<Route> {
    let registered = CLIENT.read().ok()?.clone()?;
    let session_id = session_id?;
    if !path.is_absolute() || !in_roots(path, &crate::workspace::project_dirs()) {
        return None;
    }
    Some(Route {
        fs: registered.fs,
        session_id: session_id.to_string(),
        read: registered.read,
        write: registered.write,
    })
}

impl Route {
    /// The file's text: the client's copy when it can read, the disk's when it
    /// cannot or the request failed.
    pub async fn read(&self, path: &Path) -> std::io::Result<String> {
        if self.read {
            let request = self.fs.read_text_file(&self.session_id, path);
            match tokio::time::timeout(CLIENT_FS_TIMEOUT, request).await {
                Ok(Ok(content)) => return Ok(content),
                Ok(Err(error)) => log::warn!(
                    "fs/read_text_file failed for {}: {error}; reading from disk",
                    path.display()
                ),
                Err(_) => log::warn!(
                    "fs/read_text_file timed out for {}; reading from disk",
                    path.display()
                ),
            }
        }
        std::fs::read_to_string(path)
    }

    /// Write the file's text through the client when it can write, and to the
    /// disk when it cannot or the request failed.
    pub async fn write(&self, path: &Path, content: &str) -> std::io::Result<()> {
        if self.write {
            let request = self.fs.write_text_file(&self.session_id, path, content);
            match tokio::time::timeout(CLIENT_FS_TIMEOUT, request).await {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(error)) => log::warn!(
                    "fs/write_text_file failed for {}: {error}; writing to disk",
                    path.display()
                ),
                Err(_) => log::warn!(
                    "fs/write_text_file timed out for {}; writing to disk",
                    path.display()
                ),
            }
        }
        std::fs::write(path, content)
    }
}

/// Whether `path` sits inside one of `roots`.
///
/// Both sides are resolved first, so a root reached through a symlink (macOS
/// spells `/tmp` as `/private/tmp`) still contains its files, and a `..` that
/// climbs out of a root does not pass as being inside it.
fn in_roots(path: &Path, roots: &[PathBuf]) -> bool {
    let path = resolve(path);
    roots
        .iter()
        .any(|root| path.starts_with(crate::workspace::canonical_key(root)))
}

/// `path` with its deepest existing ancestor canonicalized. A file that is
/// about to be created cannot be canonicalized itself, but its directory can.
fn resolve(path: &Path) -> PathBuf {
    let normalized = crate::workspace::normalize(path);
    let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
    let mut existing = normalized.as_path();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            return missing
                .iter()
                .rev()
                .fold(canonical, |joined, part| joined.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name);
                existing = parent;
            }
            _ => return normalized,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client holding one buffer's text, which can be told to fail.
    struct FakeClient {
        buffer: &'static str,
        fail: bool,
        written: std::sync::Mutex<Vec<(String, PathBuf, String)>>,
    }

    impl FakeClient {
        fn new(buffer: &'static str, fail: bool) -> Arc<Self> {
            Arc::new(Self {
                buffer,
                fail,
                written: std::sync::Mutex::default(),
            })
        }
    }

    #[async_trait]
    impl ClientFileSystem for FakeClient {
        async fn read_text_file(&self, _session_id: &str, _path: &Path) -> Result<String, String> {
            if self.fail {
                return Err("no such buffer".to_string());
            }
            Ok(self.buffer.to_string())
        }

        async fn write_text_file(
            &self,
            session_id: &str,
            path: &Path,
            content: &str,
        ) -> Result<(), String> {
            if self.fail {
                return Err("read-only buffer".to_string());
            }
            self.written.lock().unwrap().push((
                session_id.to_string(),
                path.to_path_buf(),
                content.to_string(),
            ));
            Ok(())
        }
    }

    fn route(client: &Arc<FakeClient>, read: bool, write: bool) -> Route {
        Route {
            fs: Arc::clone(client) as Arc<dyn ClientFileSystem>,
            session_id: "session-1".to_string(),
            read,
            write,
        }
    }

    #[tokio::test]
    async fn reads_and_writes_go_to_the_client_when_it_offers_them() {
        let dir = scratch("client");
        let file = dir.join("a.txt");
        std::fs::write(&file, "on disk").unwrap();
        let client = FakeClient::new("in the buffer", false);
        let route = route(&client, true, true);

        assert_eq!(route.read(&file).await.unwrap(), "in the buffer");
        route.write(&file, "edited").await.unwrap();

        // The client owns the write, so the disk is left for it to update.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "on disk");
        assert_eq!(
            *client.written.lock().unwrap(),
            vec![("session-1".to_string(), file.clone(), "edited".to_string())]
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_capability_the_client_did_not_offer_stays_on_disk() {
        let dir = scratch("partial");
        let file = dir.join("a.txt");
        std::fs::write(&file, "on disk").unwrap();
        let client = FakeClient::new("in the buffer", false);
        let route = route(&client, false, false);

        assert_eq!(route.read(&file).await.unwrap(), "on disk");
        route.write(&file, "edited").await.unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "edited");
        assert!(client.written.lock().unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_request_the_client_fails_falls_back_to_disk() {
        let dir = scratch("failing");
        let file = dir.join("a.txt");
        std::fs::write(&file, "on disk").unwrap();
        let client = FakeClient::new("in the buffer", true);
        let route = route(&client, true, true);

        assert_eq!(route.read(&file).await.unwrap(), "on disk");
        route.write(&file, "edited").await.unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "edited");

        std::fs::remove_dir_all(&dir).ok();
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sigit-client-fs-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_file_inside_a_root_is_in_roots() {
        let root = scratch("inside");
        std::fs::write(root.join("a.txt"), "a").unwrap();

        assert!(in_roots(&root.join("a.txt"), std::slice::from_ref(&root)));
        // Not created yet, so only its directory can be resolved.
        assert!(in_roots(
            &root.join("new").join("b.txt"),
            std::slice::from_ref(&root)
        ));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_file_outside_every_root_is_not() {
        let root = scratch("root");
        let other = scratch("other");
        std::fs::write(other.join("a.txt"), "a").unwrap();

        assert!(!in_roots(&other.join("a.txt"), std::slice::from_ref(&root)));
        // `..` out of the root names the sibling, however the path starts.
        let climbing = root
            .join("..")
            .join(other.file_name().unwrap())
            .join("a.txt");
        assert!(!in_roots(&climbing, std::slice::from_ref(&root)));
        assert!(!in_roots(&root.join("a.txt"), &[]));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_root_is_not_inside_it() {
        let root = scratch("link-root");
        let outside = scratch("link-outside");
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        assert!(!in_roots(
            &root.join("link").join("secret.txt"),
            std::slice::from_ref(&root)
        ));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }
}
