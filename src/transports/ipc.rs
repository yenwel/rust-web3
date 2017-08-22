//! IPC Transport for *nix
/*
extern crate tokio_core;
extern crate tokio_io;
#[cfg(unix)]
extern crate tokio_uds;
#[cfg(windows)]
extern crate tokio_named_pipes;

use std::{mem, thread, result};
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{self, atomic, Arc};

use self::tokio_core::reactor;
use self::tokio_io::AsyncRead;
use self::tokio_io::io::{ReadHalf, WriteHalf};
#[cfg(unix)]
use self::tokio_uds::UnixStream;
#[cfg(windows)]
use self::tokio_named_pipes::NamedPipe ;

use futures::{self, sink, Sink, Stream, Future};
use futures::sync::{oneshot, mpsc};
use helpers;
use parking_lot::Mutex;
use rpc;
use transports::Result as RpcResult;
use {BatchTransport, Transport, Error as RpcError, RequestId};

macro_rules! try_nb {
  ($e:expr) => (match $e {
    Ok(t) => t,
    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
      return Ok(futures::Async::NotReady)
    }
    Err(e) => {
      warn!("Unexpected IO error: {:?}", e);
      return Err(())
    },
  })
}

/// Event Loop Handle.
/// NOTE: Event loop is stopped when handle is dropped!
pub struct EventLoopHandle {
  thread: Option<thread::JoinHandle<()>>,
  remote: reactor::Remote,
  done: Arc<atomic::AtomicBool>,
}

impl EventLoopHandle {
  /// Returns event loop remote.
  pub fn remote(&self) -> &reactor::Remote {
    &self.remote
  }
}

impl Drop for EventLoopHandle {
  fn drop(&mut self) {
    self.done.store(true, atomic::Ordering::Relaxed);
    self.thread.take()
      .expect("We never touch thread except for drop; drop happens only once; qed")
      .join()
      .expect("Thread should shut down cleanly.");
  }
}

/// Error returned while initializing IPC transport.
pub type Error = io::Error;
/// Result of initializing IPC transport.
pub type Result<T> = result::Result<T, Error>;

type Pending = oneshot::Sender<RpcResult<Vec<RpcResult<rpc::Value>>>>;
type PendingResult = oneshot::Receiver<RpcResult<Vec<RpcResult<rpc::Value>>>>;

/// Unix Domain Sockets (IPC) transport
pub struct Ipc {
  id: atomic::AtomicUsize,
  pending: Arc<Mutex<BTreeMap<RequestId, Pending>>>,
  write_sender: Mutex<sink::Wait<mpsc::Sender<Vec<u8>>>>,
}

impl Ipc {
  /// Create new IPC transport within existing Event Loop.
  pub fn with_event_loop<P>(path: P, handle: &reactor::Handle) -> Result<Self> where
    P: AsRef<Path>,
  {
    trace!("Connecting to: {:?}", path.as_ref());
    let stream = NamedPipe::new(path.as_ref().as_os_str(), handle)?;
    Self::with_stream(stream, handle)
  }

  /// Creates new IPC transport from existing `UnixStream` and `Handle`
  fn with_stream(stream: NamedPipe, handle: &reactor::Handle) -> Result<Self> {
    let (read, write) = stream.split();
    let (write_sender, write_receiver) = mpsc::channel(1024);
    let pending = Arc::new(Mutex::new(BTreeMap::new()));

    let r = ReadStream {
      read,
      pending: pending.clone(),
      buffer: vec![],
      current_pos: 0,
    };

    let w = WriteStream {
      write,
      incoming: write_receiver,
      state: WriteState::WaitingForRequest,
    };

    handle.spawn(r);
    handle.spawn(w);

    Ok(Ipc {
      id: atomic::AtomicUsize::new(1),
      write_sender: Mutex::new(write_sender.wait()),
      pending,
    })
  }

  /// Create new IPC transport with separate event loop.
  /// NOTE: Dropping event loop handle will stop the transport layer!
  pub fn new<P>(path: P) -> Result<(EventLoopHandle, Self)> where
    P: AsRef<Path>,
  {
    let done = Arc::new(atomic::AtomicBool::new(false));
    let (tx, rx) = sync::mpsc::channel();
    let path = path.as_ref().to_owned();
    let done2 = done.clone();

    let eloop = thread::spawn(move || {
      let run = move || {
        let event_loop = reactor::Core::new()?;
        let ipc = Self::with_event_loop(path, &event_loop.handle())?;
        Ok((ipc, event_loop))
      };

      let send = move |result| {
        tx.send(result).expect("Receiving end is always waiting.");
      };

      let res: Result<_> = run();
      match res {
        Err(e) => send(Err(e)),
        Ok((ipc, mut event_loop)) => {
          send(Ok((ipc, event_loop.remote())));

          while !done2.load(atomic::Ordering::Relaxed) {
            event_loop.turn(None);
          }
        },
      }
    });

    rx.recv()
      .expect("Thread is always spawned.")
      .map(|(ipc, remote)| (
        EventLoopHandle { thread: Some(eloop), remote: remote, done: done },
        ipc,
      ))
  }
}

impl Transport for Ipc {
  type Out = IpcTask<fn (Vec<RpcResult<rpc::Value>>) -> RpcResult<rpc::Value>>;

  fn prepare(&self, method: &str, params: Vec<rpc::Value>) -> (RequestId, rpc::Call) {
    let id = self.id.fetch_add(1, atomic::Ordering::AcqRel);
    let request = helpers::build_request(id, method, params);

    (id, request)
  }

  fn send(&self, id: RequestId, request: rpc::Call) -> Self::Out {
    let request = helpers::to_string(&rpc::Request::Single(request));
    debug!("Calling: {}", request);

    let (tx, rx) = futures::oneshot();
    self.pending.lock().insert(id, tx);
    let result = {
      let mut sender = self.write_sender.lock();
      (*sender).send(request.into_bytes())
    };

    IpcTask {
      state: IpcTaskState::Sending(Some(result), rx),
      extract: single_response as fn(Vec<RpcResult<rpc::Value>>) -> RpcResult<rpc::Value>,
    }
  }
}

fn single_response(response: Vec<RpcResult<rpc::Value>>) -> RpcResult<rpc::Value> {
  match response.into_iter().next() {
    Some(res) => res,
    None => Err(RpcError::Transport(format!("Expected single response got empty batch."))),
  }
}

impl BatchTransport for Ipc {
  type Batch = IpcTask<fn(Vec<RpcResult<rpc::Value>>) -> RpcResult<Vec<RpcResult<rpc::Value>>>>;

  fn send_batch<T>(&self, requests: T) -> Self::Batch where
    T: IntoIterator<Item=(RequestId, rpc::Call)>
  {
    let mut it = requests.into_iter();
    let (id, first) = it.next().map(|x| (x.0, Some(x.1))).unwrap_or_else(|| (0, None));
    let requests = first.into_iter().chain(it.map(|x| x.1)).collect();
    let request = helpers::to_string(&rpc::Request::Batch(requests));
    debug!("Calling: {}", request);

    let (tx, rx) = futures::oneshot();
    self.pending.lock().insert(id, tx);
    let result = {
      let mut sender = self.write_sender.lock();
      (*sender).send(request.into_bytes())
    };

    IpcTask {
      state: IpcTaskState::Sending(Some(result), rx),
      extract: Ok as fn(Vec<RpcResult<rpc::Value>>) -> RpcResult<Vec<RpcResult<rpc::Value>>>,
    }
  }
}

enum IpcTaskState {
  Sending(Option<result::Result<(), mpsc::SendError<Vec<u8>>>>, PendingResult),
  WaitingForResult(PendingResult),
  Done,
}

/// A future represeting IPC transport method execution.
/// First it sends a message to writing half and waits for completion
/// and then starts to listen for expected response.
pub struct IpcTask<T> {
  state: IpcTaskState,
  extract: T,
}

impl<T, Out> Future for IpcTask<T> where
  T: Fn(Vec<RpcResult<rpc::Value>>) -> RpcResult<Out>,
{
  type Item = Out;
  type Error = RpcError;

  fn poll(&mut self) -> futures::Poll<Self::Item, Self::Error> {

    loop {
      let extract = &self.extract;
      match self.state {
        IpcTaskState::Sending(ref mut result, _) => {
          if let Some(Err(e)) = result.take() {
            return Err(RpcError::Transport(format!("{:?}", e)));
          }
        },
        IpcTaskState::WaitingForResult(ref mut rx) => {
          let result = try_ready!(rx.poll().map_err(|e| RpcError::Transport(format!("{:?}", e))));
          return result.and_then(|x| extract(x)).map(futures::Async::Ready);
        },
        IpcTaskState::Done => {
          return Err(RpcError::Unreachable);
        },
      }
      // Proceeed to the next state
      let state = mem::replace(&mut self.state, IpcTaskState::Done);
      self.state = if let IpcTaskState::Sending(_, rx) = state {
        IpcTaskState::WaitingForResult(rx)
      } else {
        state
      }
    }
  }
}

enum WriteState {
  WaitingForRequest,
  Writing {
    buffer: Vec<u8>,
    current_pos: usize
  },
}

/// Writing part of the IPC transport
/// Awaits new requests using `mpsc::Receiver` and writes them to the socket.
struct WriteStream {
  write: WriteHalf<NamedPipe>,
  incoming: mpsc::Receiver<Vec<u8>>,
  state: WriteState
}

impl Future for WriteStream {
  type Item = ();
  type Error = ();

  fn poll(&mut self) -> futures::Poll<Self::Item, Self::Error> {
    loop {
      self.state = match self.state {
        WriteState::WaitingForRequest => {
          // Ask for more to write
          let to_send = try_ready!(self.incoming.poll());
          if let Some(to_send) = to_send {
            trace!("Got new message to write: {:?}", String::from_utf8_lossy(&to_send));
            WriteState::Writing {
              buffer: to_send,
              current_pos: 0,
            }
          } else {
            return Ok(futures::Async::NotReady);
          }
        },
        WriteState::Writing { ref buffer, ref mut current_pos } => {
          // Write everything in the buffer
          while *current_pos < buffer.len() {
            let n = try_nb!(self.write.write(&buffer[*current_pos..]));
            *current_pos += n;
            if n == 0 {
              warn!("IO Error: Zero write.");
              return Err(()); // zero write?
            }
          }

          WriteState::WaitingForRequest
        },
      };
    }
  }
}
/// Reading part of the IPC transport.
/// Reads data on the socket and tries to dispatch it to awaiting requests.
struct ReadStream {
  read: ReadHalf<NamedPipe>,
  pending: Arc<Mutex<BTreeMap<RequestId, Pending>>>,
  buffer: Vec<u8>,
  current_pos: usize,
}

impl Future for ReadStream {
  type Item = ();
  type Error = ();

  fn poll(&mut self) -> futures::Poll<Self::Item, Self::Error> {
    const DEFAULT_BUF_SIZE: usize = 4096;
    let mut new_write_size = 128;
    loop {
      if self.current_pos == self.buffer.len() {
        if new_write_size < DEFAULT_BUF_SIZE {
          new_write_size *= 2;
        }
        self.buffer.resize(self.current_pos + new_write_size, 0);
      }

      let read = try_nb!(self.read.read(&mut self.buffer[self.current_pos..]));
      if read == 0 {
        return Ok(futures::Async::NotReady);
      }

      let mut min = self.current_pos;
      self.current_pos += read;
      while let Some((output, len)) = Self::extract_response(&self.buffer[0..self.current_pos], min) {
        // Respond
        self.respond(output);

        // copy rest of buffer to the beginning
        for i in len..self.current_pos {
          self.buffer.swap(i, i - len);
        }

        // truncate the buffer
        let new_len = self.current_pos - len;
        self.buffer.truncate(new_len + new_write_size);

        // Set new positions
        self.current_pos = new_len;
        min = 0;
      }
    }
  }
}

impl ReadStream {
  fn respond(&self, outputs: Vec<rpc::Output>) {
    let id = match outputs.get(0) {
      Some(&rpc::Output::Success(ref success)) => success.id.clone(),
      Some(&rpc::Output::Failure(ref failure)) => failure.id.clone(),
      None => rpc::Id::Num(0),
    };

    if let rpc::Id::Num(num) = id {
      if let Some(request) = self.pending.lock().remove(&(num as usize)) {
        trace!("Responding to (id: {:?}) with {:?}", num, outputs);
        if let Err(err) = request.send(helpers::to_results_from_outputs(outputs)) {
          warn!("Sending a response to deallocated channel: {:?}", err);
        }
      } else {
        warn!("Got response for unknown request (id: {:?})", num);
      }
    } else {
      warn!("Got unsupported response (id: {:?})", id);
    }
  }

  fn extract_response(buf: &[u8], min: usize) -> Option<(Vec<rpc::Output>, usize)> {
    for pos in (min..buf.len()).rev() {
      // Look for end character
      if buf[pos] == b']' || buf[pos] == b'}' {
        // Try to deserialize
        let pos = pos + 1;
        match helpers::to_response_from_slice(&buf[0..pos]) {
          Ok(rpc::Response::Single(output)) => return Some((vec![output], pos)),
          Ok(rpc::Response::Batch(outputs)) => return Some((outputs, pos)),
          // just continue
          _ => {},
        }
      }
    }

    None
  }
}

#[cfg(test)]
mod tests {
  extern crate tokio_core;
  extern crate tokio_uds;

  use std::io::{Read, Write};
  use super::Ipc;
  use futures::{self, Future};
  use rpc;
  use {Transport};

  #[test]
  fn should_send_a_request() {
    // given
    let mut eloop = tokio_core::reactor::Core::new().unwrap();
    let handle = eloop.handle();
    let (server, client) = tokio_named_pipes::NamedPipe::pair(&handle).unwrap();
    let ipc = Ipc::with_stream(client, &handle).unwrap();

    eloop.remote().spawn(move |_| {
      struct Task {
        server: tokio_named_pipes::NamedPipe,
      }

      impl Future for Task {
        type Item = ();
        type Error = ();
        fn poll(&mut self) -> futures::Poll<(), ()> {
          let mut data = [0; 2048];
          // Read request
          let read = self.server.read(&mut data).unwrap();
          let request = String::from_utf8(data[0..read].to_vec()).unwrap();
          assert_eq!(&request, r#"{"jsonrpc":"2.0","method":"eth_accounts","params":["1"],"id":1}"#);

          // Write response
          let response = r#"{"jsonrpc":"2.0","id":1,"result":"x"}"#;
          self.server.write_all(response.as_bytes()).unwrap();
          self.server.flush().unwrap();

          Ok(futures::Async::Ready(()))
        }
      }

      Task { server: server }
    });

    // when
    let res = ipc.execute("eth_accounts", vec![rpc::Value::String("1".into())]);

    // then
    assert_eq!(eloop.run(res), Ok(rpc::Value::String("x".into())));
  }

  #[test]
  fn should_handle_double_response() {
    // given
    let mut eloop = tokio_core::reactor::Core::new().unwrap();
    let handle = eloop.handle();
    let (server, client) = tokio_named_pipes::NamedPipe::pair(&handle).unwrap();
    let ipc = Ipc::with_stream(client, &handle).unwrap();

    eloop.remote().spawn(move |_| {
      struct Task {
        server: tokio_named_pipes::NamedPipe,
      }

      impl Future for Task {
        type Item = ();
        type Error = ();
        fn poll(&mut self) -> futures::Poll<(), ()> {
          let mut data = [0; 2048];
          // Read request
          let read = self.server.read(&mut data).unwrap();
          let request = String::from_utf8(data[0..read].to_vec()).unwrap();
          assert_eq!(&request, r#"{"jsonrpc":"2.0","method":"eth_accounts","params":["1"],"id":1}{"jsonrpc":"2.0","method":"eth_accounts","params":["1"],"id":2}"#);

          // Write response
          let response = r#"{"jsonrpc":"2.0","id":1,"result":"x"}{"jsonrpc":"2.0","id":2,"result":"x"}"#;
          self.server.write_all(response.as_bytes()).unwrap();
          self.server.flush().unwrap();

          Ok(futures::Async::Ready(()))
        }
      }

      Task { server: server }
    });

    // when
    let res1 = ipc.execute("eth_accounts", vec![rpc::Value::String("1".into())]);
    let res2 = ipc.execute("eth_accounts", vec![rpc::Value::String("1".into())]);

    // then
    assert_eq!(eloop.run(res1.join(res2)), Ok((
      rpc::Value::String("x".into()),
      rpc::Value::String("x".into())
    )));
  }
}
*/