use tower_lsp::{Client, Server};

use crate::SessionEnd;
use crate::server::NmlLanguageServer;
use crate::session::build_service;

/// Native: tower-lsp's own concurrent `Server::serve` over a detached stdin
/// (see [`detached_stdin`]), ended by `exit` or by EOF — whichever the
/// session decides first.
pub(super) async fn serve_with(init: impl FnOnce(Client) -> NmlLanguageServer) -> SessionEnd {
    let (service, socket) = build_service(init);
    let exit = service.exit_signal();
    let served = Server::new(detached_stdin(), tokio::io::stdout(), socket).serve(service);
    tokio::select! {
        // The transport ended: stdin reached EOF. If `exit` had ALREADY been
        // decided, that is the session's ending — not this one. A client that
        // sends `exit` and closes the pipe in the same breath leaves both
        // branches ready in ONE poll, and `select!` picks between ready
        // branches at random: reading the transport's ending there reported
        // `Disconnected` (code 0) for a session the protocol says is code 1.
        // Measured on the binary before this line: 15 of 60 runs exited 0.
        () = served => exit.ending().unwrap_or(SessionEnd::Disconnected),
        // `exit` arrived and the client is still holding stdin open.
        // tower-lsp's transport would read on until EOF — a client that keeps
        // stdin open after `exit` kept the process (measured) — so the
        // session ends here.
        ending = exit.ended() => ending,
    }
}

/// Stdin read on a thread of this crate's own, which the runtime does not
/// wait for.
///
/// `tokio::io::stdin()` reads on the runtime's blocking pool, and dropping
/// the runtime — which is what returning from a `#[tokio::main]` does —
/// waits for that pool with no timeout (tokio's `BlockingPool::shutdown`).
/// A thread parked in `read(2)` on a pipe the client still holds never
/// returns, so the process lived on after `exit` even though the session
/// had ended (measured on the binary, with stdin held open). A detached
/// thread is not the runtime's to wait for: when the session ends, `main`
/// returns and the process exits with the code; when the client closes
/// stdin, the read returns 0, the thread ends, and the reader reports EOF.
///
/// The cost of not waiting: that thread holds `std::io::Stdin`'s own lock
/// until the read returns, so a session that ended on `exit` leaves it held
/// for as long as the client keeps the pipe open. ONE session per process,
/// and the process ends when it returns — which is what a `<tool> lsp`
/// subcommand does. A second [`crate::serve_stdio`] beside it, or a read of stdin
/// after one returns, would wait on that lock rather than on any input.
fn detached_stdin() -> DetachedStdin {
    use std::io::Read as _;
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Vec<u8>>>(8);
    let spawned = std::thread::Builder::new()
        .name("nml-lsp stdin".to_string())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.blocking_send(Ok(buf[..n].to_vec())).is_err() {
                            break; // the reader is gone: the session ended first
                        }
                    }
                    Err(err) => {
                        let _ = tx.blocking_send(Err(err));
                        break;
                    }
                }
            }
        });
    if let Err(err) = spawned {
        // No thread, no input: the reader reports the failure as a read
        // error and the transport ends the session.
        let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Vec<u8>>>(1);
        let _ = tx.try_send(Err(err));
        return DetachedStdin {
            rx,
            pending: Vec::new(),
            at: 0,
        };
    }
    DetachedStdin {
        rx,
        pending: Vec::new(),
        at: 0,
    }
}

/// The read side of [`detached_stdin`]: chunks arrive over a channel, and
/// a closed channel is EOF.
struct DetachedStdin {
    rx: tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    pending: Vec<u8>,
    at: usize,
}

impl tokio::io::AsyncRead for DetachedStdin {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::task::Poll;
        let this = self.get_mut();
        if this.at >= this.pending.len() {
            match this.rx.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(Err(err))) => return Poll::Ready(Err(err)),
                Poll::Ready(Some(Ok(chunk))) => {
                    this.pending = chunk;
                    this.at = 0;
                }
            }
        }
        let n = buf.remaining().min(this.pending.len() - this.at);
        buf.put_slice(&this.pending[this.at..this.at + n]);
        this.at += n;
        Poll::Ready(Ok(()))
    }
}
