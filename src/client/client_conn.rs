//! Single client connection

use std::io;
use std::result::Result as std_Result;
use std::sync::Arc;

use error;
use error::Error;
use result;

use exec::CpuPoolOption;

use solicit::end_stream::EndStream;
use solicit::frame::settings::*;
use solicit::header::*;
use solicit::StreamId;
use solicit::DEFAULT_SETTINGS;

use service::Service;

use futures::future::Future;
use futures::stream::Stream;
use futures::sync::mpsc::unbounded;
use futures::sync::mpsc::UnboundedSender;
use futures::sync::oneshot;

use tls_api::TlsConnector;

use tokio_core::reactor;
use tokio_io::AsyncRead;
use tokio_io::AsyncWrite;
use tokio_timer::Timer;
use tokio_tls_api;

use solicit_async::*;

use common::*;
use data_or_trailers::*;
use socket::*;

use client_died_error_holder::ClientDiedErrorHolder;
use common::client_or_server::ClientOrServer;
use data_or_headers::DataOrHeaders;
use data_or_headers_with_flag::DataOrHeadersWithFlag;
use headers_place::HeadersPlace;
use req_resp::RequestOrResponse;
use result_or_eof::ResultOrEof;
use std::marker;
use ClientConf;
use ClientTlsOption;
use ErrorCode;

struct ClientTypes<I>(marker::PhantomData<I>);

impl<I> Types for ClientTypes<I>
where
    I: AsyncWrite + AsyncRead + Send + 'static,
{
    type Io = I;
    type HttpStreamData = ClientStream<I>;
    type HttpStreamSpecific = ClientStreamData;
    type ConnSpecific = ClientConnData;
    type ToWriteMessage = ClientToWriteMessage;

    const OUT_REQUEST_OR_RESPONSE: RequestOrResponse = RequestOrResponse::Request;

    const CLIENT_OR_SERVER: ClientOrServer = ClientOrServer::Client;
}

pub struct ClientStreamData {}

impl HttpStreamDataSpecific for ClientStreamData {}

type ClientStream<I> = HttpStreamCommon<ClientTypes<I>>;

impl<I> HttpStreamData for ClientStream<I>
where
    I: AsyncWrite + AsyncRead + Send + 'static,
{
    type Types = ClientTypes<I>;
}

pub struct ClientConnData {
    _callbacks: Box<ClientConnCallbacks>,
}

impl ConnSpecific for ClientConnData {}

pub struct ClientConn {
    write_tx: UnboundedSender<ClientToWriteMessage>,
}

unsafe impl Sync for ClientConn {}

pub struct StartRequestMessage {
    pub headers: Headers,
    pub body: HttpStreamAfterHeaders,
    pub resp_tx: oneshot::Sender<Response>,
}

enum ClientToWriteMessage {
    Start(StartRequestMessage),
    WaitForHandshake(oneshot::Sender<result::Result<()>>),
    Common(CommonToWriteMessage),
}

impl From<CommonToWriteMessage> for ClientToWriteMessage {
    fn from(m: CommonToWriteMessage) -> Self {
        ClientToWriteMessage::Common(m)
    }
}

impl<I> ConnWriteSideCustom for Conn<ClientTypes<I>>
where
    I: AsyncWrite + AsyncRead + Send + 'static,
{
    type Types = ClientTypes<I>;

    fn process_message(&mut self, message: ClientToWriteMessage) -> result::Result<()> {
        match message {
            ClientToWriteMessage::Start(start) => self.process_start(start),
            ClientToWriteMessage::Common(common) => self.process_common_message(common),
            ClientToWriteMessage::WaitForHandshake(tx) => {
                // ignore error
                drop(tx.send(Ok(())));
                Ok(())
            }
        }
    }
}

impl<I> Conn<ClientTypes<I>>
where
    I: AsyncWrite + AsyncRead + Send + 'static,
{
    fn process_start(&mut self, start: StartRequestMessage) -> result::Result<()> {
        let StartRequestMessage {
            headers,
            body,
            resp_tx,
        } = start;

        let stream_id = self.next_local_stream_id();

        let out_window = {
            let (mut http_stream, resp_stream, out_window) = self.new_stream_data(
                stream_id,
                None,
                InMessageStage::Initial,
                ClientStreamData {},
            );

            if let Err(_) = resp_tx.send(Response::from_stream(resp_stream)) {
                warn!("caller died");
            }

            http_stream.push_back(DataOrHeaders::Headers(headers));

            out_window
        };

        self.pump_stream_to_write_loop(stream_id, body.into_part_stream(), out_window);

        // Also opens latch if necessary
        self.buffer_outg_conn()?;
        Ok(())
    }
}

pub trait ClientConnCallbacks: 'static {
    // called at most once
    fn goaway(&self, stream_id: StreamId, raw_error_code: u32);
}

impl ClientConn {
    fn spawn_connected<I, C>(
        lh: reactor::Handle,
        connect: HttpFutureSend<I>,
        conf: ClientConf,
        callbacks: C,
    ) -> Self
    where
        I: AsyncWrite + AsyncRead + Send + 'static,
        C: ClientConnCallbacks,
    {
        let (to_write_tx, to_write_rx) = unbounded();

        let to_write_rx = Box::new(
            to_write_rx
                .map_err(|()| Error::IoError(io::Error::new(io::ErrorKind::Other, "to_write"))),
        );

        let c = ClientConn {
            write_tx: to_write_tx.clone(),
        };

        let settings_frame = SettingsFrame::from_settings(vec![HttpSetting::EnablePush(false)]);
        let mut settings = DEFAULT_SETTINGS;
        settings.apply_from_frame(&settings_frame);

        let handshake = connect.and_then(|conn| client_handshake(conn, settings_frame));

        let conn_died_error_holder = ClientDiedErrorHolder::new();
        let conn_died_error_holder_copy = conn_died_error_holder.clone();

        let lh_copy = lh.clone();

        let future = handshake.and_then(move |conn| {
            debug!("handshake done");

            let (read, write) = conn.split();

            let conn_data = Conn::<ClientTypes<_>>::new(
                lh_copy,
                CpuPoolOption::SingleThread,
                ClientConnData {
                    _callbacks: Box::new(callbacks),
                },
                conf.common,
                settings,
                to_write_tx.clone(),
                to_write_rx,
                read,
                write,
                conn_died_error_holder,
            );
            conn_data.run()
        });

        let future = conn_died_error_holder_copy.wrap_future(future);

        lh.spawn(future);

        c
    }

    pub fn spawn<H, C>(
        lh: reactor::Handle,
        addr: Box<ToClientStream>,
        tls: ClientTlsOption<C>,
        conf: ClientConf,
        callbacks: H,
    ) -> Self
    where
        H: ClientConnCallbacks,
        C: TlsConnector + Sync,
    {
        match tls {
            ClientTlsOption::Plain => ClientConn::spawn_plain(lh.clone(), addr, conf, callbacks),
            ClientTlsOption::Tls(domain, connector) => {
                ClientConn::spawn_tls(lh.clone(), &domain, connector, addr, conf, callbacks)
            }
        }
    }

    pub fn spawn_plain<C>(
        lh: reactor::Handle,
        addr: Box<ToClientStream>,
        conf: ClientConf,
        callbacks: C,
    ) -> Self
    where
        C: ClientConnCallbacks,
    {
        let no_delay = conf.no_delay.unwrap_or(true);
        let connect = addr.connect(&lh).map_err(Into::into);
        let map_callback = move |socket: Box<StreamItem>| {
            info!("connected to {}", addr);

            if socket.is_tcp() {
                socket
                    .set_nodelay(no_delay)
                    .expect("failed to set TCP_NODELAY");
            }

            socket
        };

        let connect: Box<Future<Item = _, Error = _> + Send> =
            if let Some(timeout) = conf.connection_timeout {
                let timer = Timer::default();
                Box::new(timer.timeout(connect, timeout).map(map_callback))
            } else {
                Box::new(connect.map(map_callback))
            };

        ClientConn::spawn_connected(lh, connect, conf, callbacks)
    }

    pub fn spawn_tls<H, C>(
        lh: reactor::Handle,
        domain: &str,
        connector: Arc<C>,
        addr: Box<ToClientStream>,
        conf: ClientConf,
        callbacks: H,
    ) -> Self
    where
        H: ClientConnCallbacks,
        C: TlsConnector + Sync,
    {
        let domain = domain.to_owned();

        let connect = addr
            .connect(&lh)
            .map(move |c| {
                info!("connected to {}", addr);
                c
            }).map_err(|e| e.into());

        let tls_conn = connect.and_then(move |conn| {
            tokio_tls_api::connect_async(&*connector, &domain, conn)
                .map_err(|e| Error::IoError(io::Error::new(io::ErrorKind::Other, e)))
        });

        let tls_conn = tls_conn.map_err(Error::from);

        ClientConn::spawn_connected(lh, Box::new(tls_conn), conf, callbacks)
    }

    pub fn start_request_with_resp_sender(
        &self,
        start: StartRequestMessage,
    ) -> Result<(), StartRequestMessage> {
        self.write_tx
            .unbounded_send(ClientToWriteMessage::Start(start))
            .map_err(|send_error| match send_error.into_inner() {
                ClientToWriteMessage::Start(start) => start,
                _ => unreachable!(),
            })
    }

    pub fn dump_state_with_resp_sender(&self, tx: oneshot::Sender<ConnStateSnapshot>) {
        let message = ClientToWriteMessage::Common(CommonToWriteMessage::DumpState(tx));
        // ignore error
        drop(self.write_tx.unbounded_send(message));
    }

    /// For tests
    #[doc(hidden)]
    pub fn _dump_state(&self) -> HttpFutureSend<ConnStateSnapshot> {
        let (tx, rx) = oneshot::channel();

        self.dump_state_with_resp_sender(tx);

        let rx =
            rx.map_err(|_| Error::from(io::Error::new(io::ErrorKind::Other, "oneshot canceled")));

        Box::new(rx)
    }

    pub fn wait_for_connect_with_resp_sender(
        &self,
        tx: oneshot::Sender<result::Result<()>>,
    ) -> std_Result<(), oneshot::Sender<result::Result<()>>> {
        self.write_tx
            .unbounded_send(ClientToWriteMessage::WaitForHandshake(tx))
            .map_err(|send_error| match send_error.into_inner() {
                ClientToWriteMessage::WaitForHandshake(tx) => tx,
                _ => unreachable!(),
            })
    }
}

impl Service for ClientConn {
    // TODO: copy-paste with Client::start_request
    fn start_request(&self, headers: Headers, body: HttpStreamAfterHeaders) -> Response {
        let (resp_tx, resp_rx) = oneshot::channel();

        let start = StartRequestMessage {
            headers: headers,
            body: body,
            resp_tx: resp_tx,
        };

        if let Err(_) = self.start_request_with_resp_sender(start) {
            return Response::err(error::Error::Other("client died"));
        }

        let resp_rx =
            resp_rx.map_err(|oneshot::Canceled| error::Error::Other("client likely died"));

        let resp_rx = resp_rx.map(|r| r.into_stream_flag());

        let resp_rx = resp_rx.flatten_stream();

        Response::from_stream(resp_rx)
    }
}

impl<I> ConnReadSideCustom for Conn<ClientTypes<I>>
where
    I: AsyncWrite + AsyncRead + Send + 'static,
{
    type Types = ClientTypes<I>;

    fn process_headers(
        &mut self,
        stream_id: StreamId,
        end_stream: EndStream,
        headers: Headers,
    ) -> result::Result<Option<HttpStreamRef<ClientTypes<I>>>> {
        let existing_stream = self
            .get_stream_for_headers_maybe_send_error(stream_id)?
            .is_some();
        if !existing_stream {
            return Ok(None);
        }

        let in_message_stage = self
            .streams
            .get_mut(stream_id)
            .unwrap()
            .stream()
            .in_message_stage;

        let headers_place = match in_message_stage {
            InMessageStage::Initial => HeadersPlace::Initial,
            InMessageStage::AfterInitialHeaders => HeadersPlace::Trailing,
            InMessageStage::AfterTrailingHeaders => {
                return Err(error::Error::InternalError(format!(
                    "closed stream must be handled before"
                )));
            }
        };

        if let Err(e) = headers.validate(RequestOrResponse::Response, headers_place) {
            warn!("invalid headers: {:?}: {:?}", e, headers);
            self.send_rst_stream(stream_id, ErrorCode::ProtocolError)?;
            return Ok(None);
        }

        let status_1xx = match headers_place {
            HeadersPlace::Initial => {
                let status = headers.status();

                let status_1xx = status >= 100 && status <= 199;
                if status_1xx && end_stream == EndStream::Yes {
                    warn!("1xx headers and end stream: {}", stream_id);
                    self.send_rst_stream(stream_id, ErrorCode::ProtocolError)?;
                    return Ok(None);
                }
                status_1xx
            }
            HeadersPlace::Trailing => {
                if end_stream == EndStream::No {
                    warn!("headers without end stream after data: {}", stream_id);
                    self.send_rst_stream(stream_id, ErrorCode::ProtocolError)?;
                    return Ok(None);
                }
                false
            }
        };

        let mut stream = self.streams.get_mut(stream_id).unwrap();
        if let Some(in_rem_content_length) = headers.content_length() {
            stream.stream().in_rem_content_length = Some(in_rem_content_length);
        }

        stream.stream().in_message_stage = match (headers_place, status_1xx) {
            (HeadersPlace::Initial, false) => InMessageStage::AfterInitialHeaders,
            (HeadersPlace::Initial, true) => InMessageStage::Initial,
            (HeadersPlace::Trailing, _) => InMessageStage::AfterTrailingHeaders,
        };

        // Ignore 1xx headers
        if !status_1xx {
            if let Some(ref mut response_handler) = stream.stream().peer_tx {
                // TODO: reset stream on error
                drop(
                    response_handler.send(ResultOrEof::Item(DataOrHeadersWithFlag {
                        content: DataOrHeaders::Headers(headers),
                        last: end_stream == EndStream::Yes,
                    })),
                );
            } else {
                // TODO: reset stream
            }
        }

        Ok(Some(stream))
    }
}
