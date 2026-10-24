use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use interprocess::local_socket::prelude::*;
use tempfile::TempDir;
use zellij_utils::consts::ipc_bind;
use zellij_utils::ipc::{
    ClientToServerMsg, IpcReceiverWithContext, IpcSenderWithContext, ServerToClientMsg,
};

pub struct FakeServer {
    _dir: TempDir,
    pub path: PathBuf,
    handle: Option<JoinHandle<Vec<ClientToServerMsg>>>,
    start_accept: Option<Sender<()>>,
    finished: Receiver<()>,
}

pub struct ServerSide {
    pub to_client: IpcSenderWithContext<ServerToClientMsg>,
    pub from_client: IpcReceiverWithContext<ClientToServerMsg>,
    received: Vec<ClientToServerMsg>,
}

impl ServerSide {
    pub fn send(&mut self, msg: ServerToClientMsg) {
        self.to_client.send_server_msg(msg).unwrap();
    }

    pub fn expect(&mut self, count: usize) {
        for _ in 0..count {
            match self.from_client.try_recv_client_msg() {
                Ok((msg, _)) => self.received.push(msg),
                Err(e) => panic!("expected a client message, got {:?}", e),
            }
        }
    }
}

impl FakeServer {
    pub fn spawn<F>(script: F) -> Self
    where
        F: FnOnce(&mut ServerSide) + Send + 'static,
    {
        let mut server = Self::spawn_deferred(script);
        server.start_accept.take().unwrap().send(()).unwrap();
        server
    }

    /// Delay accept until finish(), so tests can deterministically exercise a
    /// client that finishes writing before the server has accepted its pipes.
    pub fn spawn_deferred<F>(script: F) -> Self
    where
        F: FnOnce(&mut ServerSide) + Send + 'static,
    {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sock");
        let listener = ipc_bind(&path).unwrap();
        #[cfg(windows)]
        let reply_listener = zellij_utils::consts::ipc_bind_reply(&path).unwrap();
        let (start_accept, start) = mpsc::channel();
        let (completed, finished) = mpsc::channel();
        let handle = thread::spawn(move || {
            // Drop signals completion even if the script panics.
            let _completed = completed;
            if start.recv().is_err() {
                return Vec::new();
            }
            let stream = listener.accept().unwrap();
            #[cfg(not(windows))]
            let (to_client, from_client) = {
                let to_client: IpcSenderWithContext<ServerToClientMsg> =
                    IpcSenderWithContext::new(stream);
                let from_client: IpcReceiverWithContext<ClientToServerMsg> =
                    to_client.get_receiver();
                (to_client, from_client)
            };
            #[cfg(windows)]
            let (to_client, from_client) = {
                let reply = reply_listener.accept().unwrap();
                let to_client: IpcSenderWithContext<ServerToClientMsg> =
                    IpcSenderWithContext::new(reply);
                let from_client: IpcReceiverWithContext<ClientToServerMsg> =
                    IpcReceiverWithContext::new(stream);
                (to_client, from_client)
            };
            let mut side = ServerSide {
                to_client,
                from_client,
                received: Vec::new(),
            };
            script(&mut side);
            side.received
        });
        Self {
            _dir: dir,
            path,
            handle: Some(handle),
            start_accept: Some(start_accept),
            finished,
        }
    }

    pub fn finish(mut self) -> Vec<ClientToServerMsg> {
        if let Some(start) = self.start_accept.take() {
            let _ = start.send(());
        }
        assert_ne!(
            self.finished.recv_timeout(Duration::from_secs(10)),
            Err(RecvTimeoutError::Timeout),
            "fake server did not finish its IPC script"
        );
        self.handle.take().unwrap().join().unwrap()
    }
}
