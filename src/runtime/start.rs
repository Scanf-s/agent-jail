//! The socket on which the setup process of a created container waits for `aj start`.
//!
//! `aj start` sends `start` and a newline. The setup process starts the program and answers
//! `ok`, or `error` and a message, followed by a newline, when it knows whether the program
//! runs. It handles one start, and then stops listening, so a later request fails.

use crate::container_id::ContainerId;
use crate::state::{self, StateDir};
use anyhow::{Context, Result, anyhow, bail};
use oci_spec::runtime::ContainerState;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

const SOCKET: &str = "start.sock";
const REQUEST: &[u8] = b"start\n";
/// How long each side waits to send or to receive a request. The answer can take longer,
/// because the program can be slow to execute, for example from a network file system.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// The socket of the setup process.
pub struct StartSocket {
    listener: UnixListener,
    path: PathBuf,
}

impl StartSocket {
    /// Listen in the state directory `state_dir`.
    pub fn listen(state_dir: &StateDir) -> Result<Self> {
        let listener =
            UnixListener::bind(state_dir.socket_address(SOCKET)?).context("listen for aj start")?;
        Ok(Self {
            listener,
            path: state_dir.path().join(SOCKET),
        })
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.listener.as_fd()
    }

    /// Accept a connection and read its request. Returns the request if it asks for the start;
    /// answers any other request at once.
    pub fn accept(&self) -> Result<Option<StartRequest>> {
        let (client, _) = self.listener.accept().context("accept a connection")?;
        // A client that sends nothing must not keep the setup process from its other work.
        client
            .set_read_timeout(Some(REQUEST_TIMEOUT))
            .and_then(|()| client.set_write_timeout(Some(REQUEST_TIMEOUT)))
            .context("set the timeouts of a connection")?;
        let mut request = Vec::new();
        BufReader::new(&client)
            .take(REQUEST.len() as u64)
            .read_until(b'\n', &mut request)
            .context("read a request")?;
        let pending = StartRequest {
            client: Some(client),
        };
        if request != REQUEST {
            let request = String::from_utf8_lossy(&request);
            pending.answer(Err(format!("unknown request {request:?}")));
            return Ok(None);
        }
        Ok(Some(pending))
    }

    /// Stop listening, so that a later request fails at once.
    pub fn close(self) {
        drop(self.listener);
        let _ = fs::remove_file(&self.path);
    }
}

/// A start request that waits for its answer. Dropped without one, it tells the client that
/// the program did not start.
pub struct StartRequest {
    client: Option<UnixStream>,
}

impl StartRequest {
    pub fn answer(mut self, result: Result<(), String>) {
        if let Some(client) = self.client.take() {
            send_answer(client, result);
        }
    }
}

impl Drop for StartRequest {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            send_answer(client, Err("the program did not start".to_owned()));
        }
    }
}

fn send_answer(mut client: UnixStream, result: Result<(), String>) {
    let answer = match result {
        Ok(()) => "ok\n".to_owned(),
        Err(message) => format!("error {message}\n"),
    };
    // The client may have gone.
    let _ = client.write_all(answer.as_bytes());
}

/// Ask the setup process of the created container `id` under `root` to start its program, and
/// wait until it runs.
pub fn start(root: &Path, id: &ContainerId) -> Result<()> {
    let status = *state::read(root, id)?.oci.status();
    if status != ContainerState::Created {
        bail!("container {id} is {status}, not created");
    }
    let dir = root.join(id.as_str());
    let dir = File::open(&dir).with_context(|| format!("open {dir:?}"))?;
    let mut setup = match UnixStream::connect(state::socket_address(&dir, SOCKET)) {
        // The setup process stopped listening: another start came first, or the container
        // ended.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(not_created(root, id));
        }
        result => result.context("connect to the setup process of the container")?,
    };
    // After the request, the start cannot be cancelled, so this waits for the answer.
    let answer = (|| {
        setup.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        setup.write_all(REQUEST)?;
        let mut answer = String::new();
        setup.read_to_string(&mut answer)?;
        Ok::<_, io::Error>(answer)
    })();
    let answer = match answer {
        // The setup process stopped listening before it accepted this connection, because
        // another start came first.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
            ) =>
        {
            return Err(not_created(root, id));
        }
        result => result.context("send the start request")?,
    };
    match answer.strip_suffix('\n') {
        Some("ok") => Ok(()),
        Some(answer) if answer.starts_with("error ") => {
            Err(anyhow!("{}", &answer["error ".len()..]))
                .with_context(|| format!("start container {id}"))
        }
        // The setup process ended without an answer.
        _ => Err(not_created(root, id)),
    }
}

/// The error for a start of container `id` that the setup process did not accept.
fn not_created(root: &Path, id: &ContainerId) -> anyhow::Error {
    match state::read(root, id) {
        // Another start came first, and the program has not run yet.
        Ok(record) if *record.oci.status() == ContainerState::Created => {
            anyhow!("container {id} is already starting")
        }
        Ok(record) => anyhow!("container {id} is {}, not created", record.oci.status()),
        Err(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::random;

    struct TempSocket(StartSocket);

    impl TempSocket {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("aj-start-{:x}.sock", random::<u64>()));
            let listener = UnixListener::bind(&path).unwrap();
            Self(StartSocket { listener, path })
        }

        fn send(&self, request: &[u8]) -> UnixStream {
            let mut client = UnixStream::connect(&self.0.path).unwrap();
            client.write_all(request).unwrap();
            client
        }
    }

    impl Drop for TempSocket {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0.path);
        }
    }

    fn answer_of(mut client: UnixStream) -> String {
        let mut answer = String::new();
        client.read_to_string(&mut answer).unwrap();
        answer
    }

    #[test]
    fn answers_a_start_request() {
        let socket = TempSocket::new();
        let client = socket.send(REQUEST);

        socket.0.accept().unwrap().unwrap().answer(Ok(()));

        assert_eq!(answer_of(client), "ok\n");
    }

    #[test]
    fn reports_that_the_program_did_not_start() {
        let socket = TempSocket::new();
        let client = socket.send(REQUEST);
        let failed = socket.send(REQUEST);

        socket
            .0
            .accept()
            .unwrap()
            .unwrap()
            .answer(Err("exec failed".to_owned()));
        drop(socket.0.accept().unwrap().unwrap());

        assert_eq!(answer_of(client), "error exec failed\n");
        assert_eq!(answer_of(failed), "error the program did not start\n");
    }

    #[test]
    fn rejects_another_request() {
        let socket = TempSocket::new();
        let client = socket.send(b"stop\n");

        assert!(socket.0.accept().unwrap().is_none());

        assert_eq!(answer_of(client), "error unknown request \"stop\\n\"\n");
    }
}
