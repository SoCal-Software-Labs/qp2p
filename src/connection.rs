//! A message-oriented API wrapping the underlying QUIC library (`quinn`).

use crate::{
    config::{RetryConfig, SERVER_NAME},
    error::{ConnectionError, RecvError, RpcError, SendError, SerializationError, StreamError},
    wire_msg::WireMsg,
};
use bytes::Bytes;
use futures::{
    future,
    stream::{Stream, StreamExt, TryStream, TryStreamExt},
};
use rand::Rng;
use std::{collections::HashMap, fmt, net::SocketAddr, pin::Pin, sync::Arc, task, time::Duration};
use tokio::{
    sync::{mpsc, watch, Mutex},
    time::timeout,
};
use tracing::{error, trace, warn};

// TODO: this seems arbitrary - it may need tuned or made configurable.
const INCOMING_MESSAGE_BUFFER_LEN: usize = 10_000;

// TODO: this seems arbitrary - it may need tuned or made configurable.
const ENDPOINT_VERIFICATION_TIMEOUT: Duration = Duration::from_secs(30);

// Error reason for closing a connection when triggered manually by qp2p apis
const QP2P_CLOSED_CONNECTION: &str = "The connection was closed intentionally by qp2p.";

/// The sending API for a connection.
#[derive(Clone)]
pub struct Connection {
    inner: quinn::Connection,
    default_retry_config: Option<Arc<RetryConfig>>,
    waiting_pseudo_bi_streams: Arc<Mutex<HashMap<Bytes, mpsc::Sender<Arc<Mutex<RecvStream>>>>>>,
    // A reference to the 'alive' marker for the connection. This isn't read by `Connection`, but
    // must be held to keep background listeners alive until both halves of the connection are
    // dropped.
    _alive_tx: Arc<watch::Sender<()>>,
}

impl Connection {
    pub(crate) fn new(
        endpoint: quinn::Endpoint,
        default_retry_config: Option<Arc<RetryConfig>>,
        connection: quinn::NewConnection,
        waiting_pseudo_bi_streams: Arc<
            Mutex<HashMap<Bytes, mpsc::Sender<Arc<Mutex<RecvStream>>>>>,
        >,
    ) -> (Connection, ConnectionIncoming) {
        // this channel serves to keep the background message listener alive so long as one side of
        // the connection API is alive.
        let (alive_tx, alive_rx) = watch::channel(());
        let alive_tx = Arc::new(alive_tx);
        let peer_address = connection.connection.remote_address();

        let conn = Self {
            inner: connection.connection.clone(),
            default_retry_config,
            waiting_pseudo_bi_streams: waiting_pseudo_bi_streams.clone(),
            _alive_tx: Arc::clone(&alive_tx),
        };

        (
            conn.clone(),
            ConnectionIncoming::new(
                endpoint,
                peer_address,
                connection.uni_streams,
                connection.bi_streams,
                alive_tx,
                alive_rx,
                waiting_pseudo_bi_streams,
                connection.connection,
            ),
        )
    }

    /// A stable identifier for the connection.
    ///
    /// This ID will not change for the lifetime of the connection. Note that the connection ID will
    /// be different at each peer, so this is most useful for tracing connection activity within a
    /// single peer.
    pub fn id(&self) -> usize {
        self.inner.stable_id()
    }

    /// The address of the remote peer.
    pub fn remote_address(&self) -> SocketAddr {
        self.inner.remote_address()
    }

    /// Send a message to the peer with default retry configuration.
    ///
    /// The message will be sent on a unidirectional QUIC stream, meaning the application is
    /// responsible for correlating any anticipated responses from incoming streams.
    ///
    /// The priority will be `0` and retry behaviour will be determined by the
    /// [`Config`](crate::Config) that was used to construct the [`Endpoint`] this connection
    /// belongs to. See [`send_with`](Self::send_with) if you want to send a message with specific
    /// configuration.
    pub async fn send(&self, msg: Bytes) -> Result<(), SendError> {
        self.send_with(msg, 0, None).await
    }

    /// Send a message to the peer using the given configuration.
    ///
    /// See [`send`](Self::send) if you want to send with the default configuration.
    pub async fn send_with(
        &self,
        msg: Bytes,
        priority: i32,
        retry_config: Option<&RetryConfig>,
    ) -> Result<(), SendError> {
        match retry_config.or_else(|| self.default_retry_config.as_deref()) {
            Some(retry_config) => {
                retry_config
                    .retry(|| async {
                        self.send_uni(msg.clone(), priority)
                            .await
                            .map_err(|error| match &error {
                                // don't retry on connection loss, since we can't recover that from here
                                SendError::ConnectionLost(_) => {
                                    error!("Connection failed on send {:?}", error);
                                    backoff::Error::Permanent(error)
                                }
                                _ => backoff::Error::Transient(error),
                            })
                    })
                    .await?;
            }
            None => {
                self.send_uni(msg, priority).await?;
            }
        }
        Ok(())
    }

    /// Open a unidirection stream to the peer.
    ///
    /// Messages sent over the stream will arrive at the peer in the order they were sent.
    pub async fn open_uni(&self) -> Result<SendStream, ConnectionError> {
        let send_stream = self.inner.open_uni().await?;
        Ok(SendStream::new(send_stream))
    }

    /// Open a bidirectional stream to the peer.
    ///
    /// Bidirectional streams allow messages to be sent in both directions. This can be useful to
    /// automatically correlate response messages, for example.
    ///
    /// Messages sent over the stream will arrive at the peer in the order they were sent.
    pub async fn open_bi(&self) -> Result<(SendStream, RecvStream), ConnectionError> {
        let (send_stream, recv_stream) = self.inner.open_bi().await?;
        Ok((SendStream::new(send_stream), RecvStream::new(recv_stream)))
    }

    /// Open a pseudo-bidirectional stream to the peer.
    ///
    /// Pseudo-bidirectional streams are made up of 2 unidirectional streams. This can be useful
    /// traversing NATs
    ///
    /// Bidirectional streams allow messages to be sent in both directions. This can be useful to
    /// automatically correlate response messages, for example.
    ///
    /// Messages sent over the stream will arrive at the peer in the order they were sent.
    pub async fn open_pseudo_bi(
        &self,
    ) -> Result<(SendStream, Arc<Mutex<RecvStream>>), ConnectionError> {
        let quinn_send_stream = self.inner.open_uni().await?;
        let mut send_stream = SendStream::new(quinn_send_stream);
        let (send, mut recv) = mpsc::channel(1);
        let random_bytes_arr = rand::thread_rng().gen::<[u8; 32]>();
        let random_bytes: Bytes = random_bytes_arr.to_vec().into();
        let _ = self
            .waiting_pseudo_bi_streams
            .lock()
            .await
            .insert(random_bytes.clone(), send);

        if let Err(err) = send_stream
            .send_wire_msg(WireMsg::EndpointPseudoBiStreamReq(random_bytes))
            .await
        {
            trace!("Connection failed on send {:?}", err);
            Err(ConnectionError::Stopped)
        } else {
            if let Some(recv_stream) = recv.recv().await {
                recv.close();
                Ok((send_stream, recv_stream))
            } else {
                Err(ConnectionError::Stopped)
            }
        }
    }

    /// Close the connection immediately.
    ///
    /// This is not a graceful close - pending operations will fail immediately with
    /// [`ConnectionError::Closed`]`(`[`Close::Local`]`)`, and data on unfinished streams is not
    /// guaranteed to be delivered.
    pub fn close(&self, reason: Option<String>) {
        let reason = reason.unwrap_or_else(|| QP2P_CLOSED_CONNECTION.to_string());
        self.inner.close(0u8.into(), &reason.into_bytes());
    }

    /// Opens a uni directional stream and sends message on this stream
    async fn send_uni(&self, msg: Bytes, priority: i32) -> Result<(), SendError> {
        let mut send_stream = self.open_uni().await.map_err(SendError::ConnectionLost)?;
        send_stream.set_priority(priority);

        send_stream.send_user_msg(msg.clone()).await?;

        // We try to make sure the stream is gracefully closed and the bytes get sent, but if it
        // was already closed (perhaps by the peer) then we ignore the error.
        // TODO: we probably shouldn't ignore the error...
        send_stream.finish().await.or_else(|err| match err {
            SendError::StreamLost(StreamError::Stopped(_)) => Ok(()),
            _ => Err(err),
        })?;

        Ok(())
    }
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection")
            .field("id", &self.id())
            .field("remote_address", &self.remote_address())
            .finish_non_exhaustive()
    }
}

/// The sending API for a QUIC stream.
pub struct SendStream {
    inner: quinn::SendStream,
}

impl SendStream {
    fn new(inner: quinn::SendStream) -> Self {
        Self { inner }
    }

    /// Set the priority of the send stream.
    ///
    /// Every send stream has an initial priority of 0. Locally buffered data from streams with
    /// higher priority will be transmitted before data from streams with lower priority. Changing
    /// the priority of a stream with pending data may only take effect after that data has been
    /// transmitted. Using many different priority levels per connection may have a negative impact
    /// on performance.
    pub fn set_priority(&self, priority: i32) {
        // quinn returns `UnknownStream` error if the stream does not exist. We ignore it, on the
        // basis that operations on the stream will fail instead (and the effect of setting priority
        // or not is only observable if the stream exists).
        let _ = self.inner.set_priority(priority);
    }

    /// Send a message over the stream to the peer.
    ///
    /// Messages sent over the stream will arrive at the peer in the order they were sent.
    pub async fn send_user_msg(&mut self, msg: Bytes) -> Result<(), SendError> {
        WireMsg::UserMsg(msg).write_to_stream(&mut self.inner).await
    }

    /// Shut down the send stream gracefully.
    ///
    /// The returned future will complete once the peer has acknowledged all sent data.
    pub async fn finish(&mut self) -> Result<(), SendError> {
        self.inner.finish().await?;
        Ok(())
    }

    pub(crate) async fn send_wire_msg(&mut self, msg: WireMsg) -> Result<(), SendError> {
        msg.write_to_stream(&mut self.inner).await
    }
}

impl fmt::Debug for SendStream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("SendStream").finish_non_exhaustive()
    }
}

/// The receiving API for a bidirectional QUIC stream.
pub struct RecvStream {
    inner: quinn::RecvStream,
}

impl RecvStream {
    fn new(inner: quinn::RecvStream) -> Self {
        Self { inner }
    }

    /// Get the next message sent by the peer over this stream.
    pub async fn next(&mut self) -> Result<Option<Bytes>, RecvError> {
        // We may have duplicate EndpointPseudoBiStreamResp messages so we need to
        // loop until the first UserMsg
        loop {
            match self.next_wire_msg().await? {
                Some(WireMsg::UserMsg(msg)) => break Ok(Some(msg)),
                Some(WireMsg::EndpointPseudoBiStreamResp(_)) => {}
                None => break Ok(None),
                msg => break Err(SerializationError::unexpected(&msg).into()),
            }
        }
    }

    pub(crate) async fn next_wire_msg(&mut self) -> Result<Option<WireMsg>, RecvError> {
        WireMsg::read_from_stream(&mut self.inner).await
    }
}

impl fmt::Debug for RecvStream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("RecvStream").finish_non_exhaustive()
    }
}

/// The receiving API for a connection.
#[derive(Debug)]
pub struct ConnectionIncoming {
    message_rx: mpsc::Receiver<
        Result<
            (
                Bytes,
                Arc<Mutex<RecvStream>>,
                Option<Arc<Mutex<SendStream>>>,
            ),
            RecvError,
        >,
    >,
    _alive_tx: Arc<watch::Sender<()>>,
}

impl ConnectionIncoming {
    fn new(
        endpoint: quinn::Endpoint,
        peer_addr: SocketAddr,
        uni_streams: quinn::IncomingUniStreams,
        bi_streams: quinn::IncomingBiStreams,
        alive_tx: Arc<watch::Sender<()>>,
        alive_rx: watch::Receiver<()>,
        waiting_pseudo_bi_streams: Arc<
            Mutex<HashMap<Bytes, mpsc::Sender<Arc<Mutex<RecvStream>>>>>,
        >,
        connection: quinn::Connection,
    ) -> Self {
        let (message_tx, message_rx) = mpsc::channel(INCOMING_MESSAGE_BUFFER_LEN);

        // offload the actual message handling to a background task - the task will exit when
        // `alive_tx` is dropped, which would be when both sides of the connection are dropped.
        start_message_listeners(
            endpoint,
            peer_addr,
            uni_streams,
            bi_streams,
            alive_rx,
            message_tx,
            waiting_pseudo_bi_streams,
            connection,
        );

        Self {
            message_rx,
            _alive_tx: alive_tx,
        }
    }

    /// Get the next stream along with its first message and its SendStream if it is bidirectional
    pub async fn next_stream(
        &mut self,
    ) -> Result<
        Option<(
            Bytes,
            Arc<Mutex<RecvStream>>,
            Option<Arc<Mutex<SendStream>>>,
        )>,
        RecvError,
    > {
        self.message_rx.recv().await.transpose()
    }
}

// Start listeners in background tokio tasks. These tasks will run until they terminate, which would
// be when the connection terminates, or all connection handles are dropped.
//
// `alive_tx` is used to detect when all connection handles are dropped.
// `message_tx` is used to exfiltrate messages and stream errors.
fn start_message_listeners(
    endpoint: quinn::Endpoint,
    peer_addr: SocketAddr,
    uni_streams: quinn::IncomingUniStreams,
    bi_streams: quinn::IncomingBiStreams,
    alive_rx: watch::Receiver<()>,
    message_tx: mpsc::Sender<
        Result<
            (
                Bytes,
                Arc<Mutex<RecvStream>>,
                Option<Arc<Mutex<SendStream>>>,
            ),
            RecvError,
        >,
    >,
    waiting_pseudo_bi_streams: Arc<Mutex<HashMap<Bytes, mpsc::Sender<Arc<Mutex<RecvStream>>>>>>,
    connection: quinn::Connection,
) {
    let _ = tokio::spawn(listen_on_uni_streams(
        peer_addr,
        FilterBenignClose(uni_streams),
        alive_rx.clone(),
        message_tx.clone(),
        waiting_pseudo_bi_streams,
        connection,
    ));

    let _ = tokio::spawn(listen_on_bi_streams(
        endpoint,
        peer_addr,
        FilterBenignClose(bi_streams),
        alive_rx,
        message_tx,
    ));
}

async fn listen_on_uni_streams(
    peer_addr: SocketAddr,
    uni_streams: FilterBenignClose<quinn::IncomingUniStreams>,
    mut alive_rx: watch::Receiver<()>,
    message_tx: mpsc::Sender<
        Result<
            (
                Bytes,
                Arc<Mutex<RecvStream>>,
                Option<Arc<Mutex<SendStream>>>,
            ),
            RecvError,
        >,
    >,
    waiting_pseudo_bi_streams: Arc<Mutex<HashMap<Bytes, mpsc::Sender<Arc<Mutex<RecvStream>>>>>>,
    connection: quinn::Connection,
) {
    trace!(
        "Started listener for incoming uni-streams from {}",
        peer_addr
    );

    let streaming = uni_streams.try_for_each_concurrent(None, |mut recv_stream| {
        let message_tx = &message_tx;
        let connection = &connection;
        let waiting_pseudo_bi_streams = &waiting_pseudo_bi_streams;
        async move {
            trace!("Handling incoming bi-stream from {}", peer_addr);

            match WireMsg::read_from_stream(&mut recv_stream).await {
                Err(error) => {
                    if let Err(error) = message_tx.send(Err(error)).await {
                        // if we can't send the result, the receiving end is closed so we should stop
                        trace!("Receiver gone, dropping error: {:?}", error);
                    }
                }
                Ok(None) => {}
                Ok(Some(WireMsg::UserMsg(msg))) => {
                    let recv_arc_mutex = Arc::new(Mutex::new(RecvStream::new(recv_stream)));
                    if let Err(error) = message_tx.send(Ok((msg, recv_arc_mutex, None))).await {
                        // if we can't send the result, the receiving end is closed so we should stop
                        trace!("Receiver gone, dropping error: {:?}", error);
                    }
                }
                Ok(Some(WireMsg::EndpointPseudoBiStreamReq(id))) => {
                    if let Err(error) =
                        consume_pseudo_req_messages(id, recv_stream, connection, message_tx).await
                    {
                        trace!("Pseudo BiStream Request, dropping error: {:?}", error);
                    }
                }
                Ok(Some(WireMsg::EndpointPseudoBiStreamResp(id))) => {
                    if let Err(error) =
                        consume_pseudo_resp_messages(id, recv_stream, waiting_pseudo_bi_streams)
                            .await
                    {
                        trace!("Pseudo BiStream Request, dropping error: {:?}", error);
                    }
                }
                Ok(msg) => {
                    if let Err(error) = message_tx
                        .send(Err(SerializationError::unexpected(&msg).into()))
                        .await
                    {
                        // if we can't send the result, the receiving end is closed so we should stop
                        trace!("Receiver gone, dropping error: {:?}", error);
                    }
                }
            }

            Ok(())
        }
    });

    // it's a shame to allocate, but there are `Pin` errors otherwise – and we should only be doing
    // this once.
    let mut alive = Box::pin(alive_rx.changed());

    match future::select(streaming, &mut alive).await {
        future::Either::Left((Ok(()), _)) => {
            trace!(
                "Stopped listener for incoming uni-streams from {}: stream ended",
                peer_addr
            );
        }
        future::Either::Left((Err(error), _)) => {
            warn!(
                "Stopped listener for incoming uni-streams from {} due to error: {:?}",
                peer_addr, error
            );
        }
        future::Either::Right((_, _)) => {
            // the connection was closed
            // TODO: should we just drop pending messages here? if not, how long do we wait?
            trace!(
                "Stopped listener for incoming uni-streams from {}: connection handles dropped",
                peer_addr
            );
        }
    }
}

async fn consume_pseudo_req_messages(
    id: Bytes,
    recv_stream: quinn::RecvStream,
    connection: &quinn::Connection,
    message_tx: &mpsc::Sender<
        Result<
            (
                Bytes,
                Arc<Mutex<RecvStream>>,
                Option<Arc<Mutex<SendStream>>>,
            ),
            RecvError,
        >,
    >,
) -> Result<(), SendError> {
    let mut recv_stream = recv_stream;
    let inner_send_stream = connection.open_uni().await?;
    let mut send_stream = SendStream::new(inner_send_stream);
    send_stream
        .send_wire_msg(WireMsg::EndpointPseudoBiStreamResp(id))
        .await?;

    // Loop until we hit a UserMsg in case duplicate messages are sent
    loop {
        match WireMsg::read_from_stream(&mut recv_stream).await {
            Err(error) => {
                trace!("Endpoint message loop, dropping error: {:?}", error);
                break;
            }
            Ok(None) => break,
            Ok(Some(WireMsg::UserMsg(msg))) => {
                let recv_arc_mutex = Arc::new(Mutex::new(RecvStream::new(recv_stream)));
                let send_arc_mutex = Arc::new(Mutex::new(send_stream));

                if let Err(error) = message_tx
                    .send(Ok((msg, recv_arc_mutex, Some(send_arc_mutex))))
                    .await
                {
                    // if we can't send the result, the receiving end is closed so we should stop
                    trace!("Receiver gone, dropping error: {:?}", error);
                }

                break;
            }
            Ok(Some(_)) => {
                continue;
            }
        }
    }

    Ok(())
}

async fn consume_pseudo_resp_messages(
    id: Bytes,
    recv_stream: quinn::RecvStream,
    waiting_pseudo_bi_streams: &Arc<Mutex<HashMap<Bytes, mpsc::Sender<Arc<Mutex<RecvStream>>>>>>,
) -> Result<(), SendError> {
    let recv_arc_mutex = Arc::new(Mutex::new(RecvStream::new(recv_stream)));
    let mut map = waiting_pseudo_bi_streams.lock().await;
    if let Some(sender) = map.get(&id) {
        if let Err(err) = sender.try_send(recv_arc_mutex) {
            trace!("Could not send stream to waiting Pseudo BiStream {:?}", err)
        };
        let _ = map.remove(&id);
    }

    Ok(())
}

async fn listen_on_bi_streams(
    endpoint: quinn::Endpoint,
    peer_addr: SocketAddr,
    bi_streams: FilterBenignClose<quinn::IncomingBiStreams>,
    mut alive_rx: watch::Receiver<()>,
    message_tx: mpsc::Sender<
        Result<
            (
                Bytes,
                Arc<Mutex<RecvStream>>,
                Option<Arc<Mutex<SendStream>>>,
            ),
            RecvError,
        >,
    >,
) {
    trace!(
        "Started listener for incoming bi-streams from {}",
        peer_addr
    );

    let streaming =
        bi_streams.try_for_each_concurrent(None, |(mut send_stream, mut recv_stream)| {
            let endpoint = &endpoint;
            let message_tx = &message_tx;
            async move {
                trace!("Handling incoming bi-stream from {}", peer_addr);

                match WireMsg::read_from_stream(&mut recv_stream).await {
                    Err(error) => {
                        if let Err(error) = message_tx.send(Err(error)).await {
                            // if we can't send the result, the receiving end is closed so we should stop
                            trace!("Receiver gone, dropping error: {:?}", error);
                        }
                    }
                    Ok(None) => {}
                    Ok(Some(WireMsg::UserMsg(msg))) => {
                        let send_arc_mutex = Arc::new(Mutex::new(SendStream::new(send_stream)));
                        let recv_arc_mutex = Arc::new(Mutex::new(RecvStream::new(recv_stream)));

                        if let Err(error) = message_tx
                            .send(Ok((msg, recv_arc_mutex, Some(send_arc_mutex))))
                            .await
                        {
                            // if we can't send the result, the receiving end is closed so we should stop
                            trace!("Receiver gone, dropping error: {:?}", error);
                        }
                    }
                    Ok(Some(wire_msg)) => {
                        if let Err(error) = handle_endpoint_message(
                            &endpoint,
                            peer_addr,
                            wire_msg,
                            &mut recv_stream,
                            &mut send_stream,
                        )
                        .await
                        {
                            // if we can't send the result, the receiving end is closed so we should stop
                            trace!("Endpoint message, dropping error: {:?}", error);
                        }
                    }
                }

                Ok(())
            }
        });

    // it's a shame to allocate, but there are `Pin` errors otherwise – and we should only be doing
    // this once.
    let mut alive = Box::pin(alive_rx.changed());

    match future::select(streaming, &mut alive).await {
        future::Either::Left((Ok(()), _)) => {
            trace!(
                "Stopped listener for incoming bi-streams from {}: stream ended",
                peer_addr
            );
        }
        future::Either::Left((Err(error), _)) => {
            // A connection error occurred on bi_streams, we don't propagate anything here as we
            // expect propagation to be handled in listen_on_uni_streams.
            warn!(
                "Stopped listener for incoming bi-streams from {} due to error: {:?}",
                peer_addr, error
            );
        }
        future::Either::Right((_, _)) => {
            // the connection was closed
            // TODO: should we just drop pending messages here? if not, how long do we wait?
            trace!(
                "Stopped listener for incoming bi-streams from {}: connection handles dropped",
                peer_addr
            );
        }
    }
}

async fn handle_endpoint_message(
    endpoint: &quinn::Endpoint,
    peer_addr: SocketAddr,
    first_msg: WireMsg,
    recv_stream: &mut quinn::RecvStream,
    send_stream: &mut quinn::SendStream,
) -> Result<(), SendError> {
    let mut msg = first_msg;

    loop {
        match msg {
            WireMsg::EndpointEchoReq => {
                if let Err(error) = handle_endpoint_echo(send_stream, peer_addr).await {
                    // TODO: consider more carefully how to handle this
                    warn!("Error handling endpoint echo request: {}", error);
                }
            }
            WireMsg::EndpointVerificationReq(addr) => {
                if let Err(error) = handle_endpoint_verification(endpoint, send_stream, addr).await
                {
                    // TODO: consider more carefully how to handle this
                    warn!("Error handling endpoint verification request: {}", error);
                }
            }
            other_msg => {
                // TODO: consider more carefully how to handle this
                warn!(
                    "Error on bi-stream: {}",
                    SerializationError::unexpected(&Some(other_msg))
                );
            }
        }
        match WireMsg::read_from_stream(recv_stream).await {
            Err(error) => {
                trace!("Endpoint message loop, dropping error: {:?}", error);
                break;
            }
            Ok(None) => break,
            Ok(Some(new_msg)) => {
                msg = new_msg;
            }
        }
    }

    Ok(())
}

async fn handle_endpoint_echo(
    send_stream: &mut quinn::SendStream,
    peer_addr: SocketAddr,
) -> Result<(), SendError> {
    trace!("Replying to EndpointEchoReq from {}", peer_addr);
    WireMsg::EndpointEchoResp(peer_addr)
        .write_to_stream(send_stream)
        .await
}

async fn handle_endpoint_verification(
    endpoint: &quinn::Endpoint,
    send_stream: &mut quinn::SendStream,
    addr: SocketAddr,
) -> Result<(), SendError> {
    trace!("Performing endpoint verification for {}", addr);

    let verify = async {
        trace!(
            "EndpointVerificationReq: opening new connection to {}",
            addr
        );
        let connection = endpoint
            .connect(addr, SERVER_NAME)
            .map_err(ConnectionError::from)?
            .await?;

        let (mut send_stream, mut recv_stream) = connection.connection.open_bi().await?;
        trace!(
            "EndpointVerificationReq: sending EndpointEchoReq to {} over connection {}",
            addr,
            connection.connection.stable_id()
        );
        WireMsg::EndpointEchoReq
            .write_to_stream(&mut send_stream)
            .await?;

        match WireMsg::read_from_stream(&mut recv_stream).await? {
            Some(WireMsg::EndpointEchoResp(_)) => {
                trace!(
                    "EndpointVerificationReq: Received EndpointEchoResp from {}",
                    addr
                );
                Ok(())
            }
            msg => Err(RecvError::from(SerializationError::unexpected(&msg)).into()),
        }
    };

    let verified: Result<_, RpcError> = timeout(ENDPOINT_VERIFICATION_TIMEOUT, verify)
        .await
        .unwrap_or_else(|error| Err(error.into()));

    if let Err(error) = &verified {
        warn!("Endpoint verification for {} failed: {:?}", addr, error);
    }

    WireMsg::EndpointVerificationResp(verified.is_ok())
        .write_to_stream(send_stream)
        .await?;

    Ok(())
}

struct FilterBenignClose<S>(S);

impl<S> Stream for FilterBenignClose<S>
where
    S: Stream<Item = Result<S::Ok, S::Error>> + TryStream + Unpin,
    S::Error: Into<ConnectionError>,
{
    type Item = Result<S::Ok, ConnectionError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        ctx: &mut task::Context,
    ) -> task::Poll<Option<Self::Item>> {
        let next = futures::ready!(self.0.poll_next_unpin(ctx));
        task::Poll::Ready(match next.transpose() {
            Ok(next) => next.map(Ok),
            Err(error) => {
                let error = error.into();
                if error.is_benign() {
                    warn!("Benign error ignored {:?}", error);
                    None
                } else {
                    Some(Err(error))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Connection;
    use crate::{
        config::{Config, InternalConfig, SERVER_NAME},
        error::{ConnectionError, SendError},
        tests::local_addr,
        wire_msg::WireMsg,
    };
    use bytes::Bytes;
    use color_eyre::eyre::{bail, Result};
    use futures::{StreamExt, TryStreamExt};
    use quinn::Endpoint as QuinnEndpoint;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn basic_usage() -> Result<()> {
        let config = InternalConfig::try_from_config(Default::default())?;

        let (mut peer1, _peer1_incoming) =
            QuinnEndpoint::server(config.server.clone(), local_addr())?;
        peer1.set_default_client_config(config.client);

        let (peer2, peer2_incoming) = QuinnEndpoint::server(config.server.clone(), local_addr())?;

        {
            let (p1_tx, mut p1_rx) = Connection::new(
                peer1.clone(),
                None,
                peer1.connect(peer2.local_addr()?, SERVER_NAME)?.await?,
                Arc::new(Mutex::new(HashMap::new())),
            );

            let (p2_tx, mut p2_rx) =
                if let Some(connection) = timeout(peer2_incoming.then(|c| c).try_next()).await?? {
                    Connection::new(
                        peer2.clone(),
                        None,
                        connection,
                        Arc::new(Mutex::new(HashMap::new())),
                    )
                } else {
                    bail!("did not receive incoming connection when one was expected");
                };

            p1_tx
                .open_uni()
                .await?
                .send_user_msg(Bytes::from_static(b"hello"))
                .await?;

            if let Some((msg, _, _)) = timeout(p2_rx.next_stream()).await?? {
                assert_eq!(&msg[..], b"hello");
            } else {
                bail!("did not receive message when one was expected");
            }

            p2_tx
                .open_uni()
                .await?
                .send_user_msg(Bytes::from_static(b"world"))
                .await?;

            if let Some((msg, _, _)) = timeout(p1_rx.next_stream()).await?? {
                assert_eq!(&msg[..], b"world");
            } else {
                bail!("did not receive message when one was expected");
            }
        }

        // check the connections were shutdown on drop
        timeout(peer1.wait_idle()).await?;
        timeout(peer2.wait_idle()).await?;

        Ok(())
    }

    #[tokio::test]
    async fn benign_connection_loss() -> Result<()> {
        let config = InternalConfig::try_from_config(Config {
            // set a very low idle timeout
            idle_timeout: Some(Duration::from_secs(1)),
            ..Default::default()
        })?;

        let (mut peer1, _peer1_incoming) =
            QuinnEndpoint::server(config.server.clone(), local_addr())?;
        peer1.set_default_client_config(config.client);

        let (peer2, peer2_incoming) = QuinnEndpoint::server(config.server.clone(), local_addr())?;

        // open a connection between the two peers
        let (p1_tx, _) = Connection::new(
            peer1.clone(),
            None,
            peer1.connect(peer2.local_addr()?, SERVER_NAME)?.await?,
            Arc::new(Mutex::new(HashMap::new())),
        );

        let (_, mut p2_rx) =
            if let Some(connection) = timeout(peer2_incoming.then(|c| c).try_next()).await?? {
                Connection::new(
                    peer2.clone(),
                    None,
                    connection,
                    Arc::new(Mutex::new(HashMap::new())),
                )
            } else {
                bail!("did not receive incoming connection when one was expected");
            };

        // let 2 * idle timeout pass
        tokio::time::sleep(Duration::from_secs(2)).await;

        // trying to send a message should fail with an error
        match p1_tx.send(b"hello"[..].into()).await {
            Err(SendError::ConnectionLost(ConnectionError::TimedOut)) => {}
            res => bail!("unexpected send result: {:?}", res),
        }

        // trying to receive should NOT return an error
        match p2_rx.next_stream().await {
            Ok(None) => {}
            res => bail!("unexpected recv result: {:?}", res),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_endpoint_echo() -> Result<()> {
        let config = InternalConfig::try_from_config(Config::default())?;

        let (mut peer1, _peer1_incoming) =
            QuinnEndpoint::server(config.server.clone(), local_addr())?;
        peer1.set_default_client_config(config.client);

        let (peer2, peer2_incoming) = QuinnEndpoint::server(config.server.clone(), local_addr())?;

        {
            let (p1_tx, _) = Connection::new(
                peer1.clone(),
                None,
                peer1.connect(peer2.local_addr()?, SERVER_NAME)?.await?,
                Arc::new(Mutex::new(HashMap::new())),
            );

            // we need to accept the connection on p2, or the message won't be processed
            let _p2_handle =
                if let Some(connection) = timeout(peer2_incoming.then(|c| c).try_next()).await?? {
                    Connection::new(
                        peer2.clone(),
                        None,
                        connection,
                        Arc::new(Mutex::new(HashMap::new())),
                    )
                } else {
                    bail!("did not receive incoming connection when one was expected");
                };

            let (mut send_stream, mut recv_stream) = p1_tx.open_bi().await?;
            send_stream.send_wire_msg(WireMsg::EndpointEchoReq).await?;

            if let Some(msg) = timeout(recv_stream.next_wire_msg()).await?? {
                if let WireMsg::EndpointEchoResp(addr) = msg {
                    assert_eq!(addr, peer1.local_addr()?);
                } else {
                    bail!(
                        "received unexpected message when EndpointEchoResp was expected: {:?}",
                        msg
                    );
                }
            } else {
                bail!("did not receive incoming message when one was expected");
            }
        }

        // check the connections were shutdown on drop
        timeout(peer1.wait_idle()).await?;
        timeout(peer2.wait_idle()).await?;

        Ok(())
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn endpoint_verification() -> Result<()> {
        let config = InternalConfig::try_from_config(Default::default())?;

        let (mut peer1, peer1_incoming) =
            QuinnEndpoint::server(config.server.clone(), local_addr())?;
        peer1.set_default_client_config(config.client.clone());

        let (mut peer2, peer2_incoming) =
            QuinnEndpoint::server(config.server.clone(), local_addr())?;
        peer2.set_default_client_config(config.client);

        {
            let (p1_tx, _) = Connection::new(
                peer1.clone(),
                None,
                peer1.connect(peer2.local_addr()?, SERVER_NAME)?.await?,
                Arc::new(Mutex::new(HashMap::new())),
            );

            // we need to accept the connection on p2, or the message won't be processed
            let _p2_handle =
                if let Some(connection) = timeout(peer2_incoming.then(|c| c).try_next()).await?? {
                    Connection::new(
                        peer2.clone(),
                        None,
                        connection,
                        Arc::new(Mutex::new(HashMap::new())),
                    )
                } else {
                    bail!("did not receive incoming connection when one was expected");
                };

            let (mut send_stream, mut recv_stream) = p1_tx.open_bi().await?;
            send_stream
                .send_wire_msg(WireMsg::EndpointVerificationReq(peer1.local_addr()?))
                .await?;

            // we need to accept the connection on p1, or the message won't be processed
            let _p1_handle =
                if let Some(connection) = timeout(peer1_incoming.then(|c| c).try_next()).await?? {
                    Connection::new(
                        peer1.clone(),
                        None,
                        connection,
                        Arc::new(Mutex::new(HashMap::new())),
                    )
                } else {
                    bail!("did not receive incoming connection when one was expected");
                };

            if let Some(msg) = timeout(recv_stream.next_wire_msg()).await?? {
                if let WireMsg::EndpointVerificationResp(true) = msg {
                } else {
                    bail!(
                        "received unexpected message when EndpointVerificationResp(true) was expected: {:?}",
                        msg
                    );
                }
            } else {
                bail!("did not receive incoming message when one was expected");
            }

            send_stream
                .send_wire_msg(WireMsg::EndpointVerificationReq(local_addr()))
                .await?;

            if let Some(msg) = timeout(recv_stream.next_wire_msg()).await?? {
                if let WireMsg::EndpointVerificationResp(false) = msg {
                } else {
                    bail!(
                        "received unexpected message when EndpointVerificationResp(false) was expected: {:?}",
                        msg
                    );
                }
            } else {
                bail!("did not receive incoming message when one was expected");
            }
        }

        // check the connections were shutdown on drop
        timeout(peer1.wait_idle()).await?;
        timeout(peer2.wait_idle()).await?;

        Ok(())
    }

    async fn timeout<F: std::future::Future>(
        f: F,
    ) -> Result<F::Output, tokio::time::error::Elapsed> {
        tokio::time::timeout(Duration::from_millis(500), f).await
    }
}
