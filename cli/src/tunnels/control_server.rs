/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/
use crate::async_pipe::get_socket_rw_stream;
use crate::constants::{CONTROL_PORT, PRODUCT_NAME_LONG};
use crate::log;
use crate::msgpack_rpc::U32PrefixedCodec;
use crate::rpc::{MaybeSync, RpcBuilder, RpcDispatcher, Serialization};
use crate::self_update::SelfUpdate;
use crate::state::LauncherPaths;
use crate::tunnels::protocol::HttpRequestParams;
use crate::tunnels::socket_signal::CloseReason;
use crate::update_service::{Platform, Release, TargetKind, UpdateService};
use crate::util::errors::{
	wrap, AnyError, CodeError, InvalidRpcDataError, MismatchedLaunchModeError,
	NoAttachedServerError,
};
use crate::util::http::{
	DelegatedHttpRequest, DelegatedSimpleHttp, FallbackSimpleHttp, ReqwestSimpleHttp,
};
use crate::util::io::SilentCopyProgress;
use crate::util::is_integrated_cli;
use crate::util::sync::{new_barrier, Barrier};

use futures::stream::FuturesUnordered;
use futures::FutureExt;
use opentelemetry::trace::SpanKind;
use opentelemetry::KeyValue;
use std::collections::HashMap;
use std::process::Stdio;
use tokio::pin;
use tokio_util::codec::Decoder;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::{mpsc, Mutex};

use super::code_server::{
	download_cli_into_cache, AnyCodeServer, CodeServerArgs, ServerBuilder, ServerParamsRaw,
	SocketCodeServer,
};
use super::dev_tunnels::ActiveTunnel;
use super::paths::prune_stopped_servers;
use super::port_forwarder::{PortForwarding, PortForwardingProcessor};
use super::protocol::{
	AcquireCliParams, CallServerHttpParams, CallServerHttpResult, ClientRequestMethod, EmptyObject,
	ForwardParams, ForwardResult, GetHostnameResponse, HttpBodyParams, HttpHeadersParams,
	ServeParams, ServerLog, ServerMessageParams, SpawnParams, SpawnResult, ToClientRequest,
	UnforwardParams, UpdateParams, UpdateResult, VersionParams,
};
use super::server_bridge::ServerBridge;
use super::server_multiplexer::ServerMultiplexer;
use super::shutdown_signal::ShutdownSignal;
use super::socket_signal::{
	ClientMessageDecoder, ServerMessageDestination, ServerMessageSink, SocketSignal,
};

type HttpRequestsMap = Arc<std::sync::Mutex<HashMap<u32, DelegatedHttpRequest>>>;
type CodeServerCell = Arc<Mutex<Option<SocketCodeServer>>>;

struct HandlerContext {
	/// Log handle for the server
	log: log::Logger,
	/// Whether the server update during the handler session.
	did_update: Arc<AtomicBool>,
	/// A loopback channel to talk to the socket server task.
	socket_tx: mpsc::Sender<SocketSignal>,
	/// Configured launcher paths.
	launcher_paths: LauncherPaths,
	/// Connected VS Code Server
	code_server: CodeServerCell,
	/// Potentially many "websocket" connections to client
	server_bridges: ServerMultiplexer,
	// the cli arguments used to start the code server
	code_server_args: CodeServerArgs,
	/// port forwarding functionality
	port_forwarding: PortForwarding,
	/// install platform for the VS Code server
	platform: Platform,
	/// http client to make download/update requests
	http: Arc<FallbackSimpleHttp>,
	/// requests being served by the client
	http_requests: HttpRequestsMap,
}

static MESSAGE_ID_COUNTER: AtomicU32 = AtomicU32::new(0);

// Gets a next incrementing number that can be used in logs
pub fn next_message_id() -> u32 {
	MESSAGE_ID_COUNTER.fetch_add(1, Ordering::SeqCst)
}

impl HandlerContext {
	async fn dispose(&self) {
		self.server_bridges.dispose().await;
		info!(self.log, "Disposed of connection to running server.");
	}
}

enum ServerSignal {
	/// Signalled when the server has been updated and we want to respawn.
	/// We'd generally need to stop and then restart the launcher, but the
	/// program might be managed by a supervisor like systemd. Instead, we
	/// will stop the TCP listener and spawn the launcher again as a subprocess
	/// with the same arguments we used.
	Respawn,
}

pub enum Next {
	/// Whether the server should be respawned in a new binary (see ServerSignal.Respawn).
	Respawn,
	/// Whether the tunnel should be restarted
	Restart,
	/// Whether the process should exit
	Exit,
}

pub struct ServerTermination {
	pub next: Next,
	pub tunnel: ActiveTunnel,
}

// Runs the launcher server. Exits on a ctrl+c or when requested by a user.
// Note that client connections may not be closed when this returns; use
// `close_all_clients()` on the ServerTermination to make this happen.
pub async fn serve(
	log: &log::Logger,
	mut tunnel: ActiveTunnel,
	launcher_paths: &LauncherPaths,
	code_server_args: &CodeServerArgs,
	platform: Platform,
	mut shutdown_rx: Barrier<ShutdownSignal>,
) -> Result<ServerTermination, AnyError> {
	let mut port = tunnel.add_port_direct(CONTROL_PORT).await?;
	let mut forwarding = PortForwardingProcessor::new();
	let (tx, mut rx) = mpsc::channel::<ServerSignal>(4);
	let (exit_barrier, signal_exit) = new_barrier();

	loop {
		tokio::select! {
			Ok(reason) = shutdown_rx.wait() => {
				info!(log, "Shutting down: {}", reason);
				drop(signal_exit);
				return Ok(ServerTermination {
					next: match reason {
						ShutdownSignal::RpcRestartRequested => Next::Restart,
						_ => Next::Exit,
					},
					tunnel,
				});
			},
			c = rx.recv() => {
				if let Some(ServerSignal::Respawn) = c {
					drop(signal_exit);
					return Ok(ServerTermination {
						next: Next::Respawn,
						tunnel,
					});
				}
			},
			Some(w) = forwarding.recv() => {
				forwarding.process(w, &mut tunnel).await;
			},
			l = port.recv() => {
				let socket = match l {
					Some(p) => p,
					None => {
						warning!(log, "ssh tunnel disposed, tearing down");
						return Ok(ServerTermination {
							next: Next::Restart,
							tunnel,
						});
					}
				};

				let own_log = log.prefixed(&log::new_rpc_prefix());
				let own_tx = tx.clone();
				let own_paths = launcher_paths.clone();
				let own_exit = exit_barrier.clone();
				let own_code_server_args = code_server_args.clone();
				let own_forwarding = forwarding.handle();

				tokio::spawn(async move {
					use opentelemetry::trace::{FutureExt, TraceContextExt};

					let span = own_log.span("server.socket").with_kind(SpanKind::Consumer).start(own_log.tracer());
					let cx = opentelemetry::Context::current_with_span(span);
					let serve_at = Instant::now();

					debug!(own_log, "Serving new connection");

					let (writehalf, readhalf) = socket.into_split();
					let stats = process_socket(own_exit, readhalf, writehalf, own_log, own_tx, own_paths, own_code_server_args, own_forwarding, platform).with_context(cx.clone()).await;

					cx.span().add_event(
						"socket.bandwidth",
						vec![
							KeyValue::new("tx", stats.tx as f64),
							KeyValue::new("rx", stats.rx as f64),
							KeyValue::new("duration_ms", serve_at.elapsed().as_millis() as f64),
						],
					);
					cx.span().end();
				   });
			}
		}
	}
}

struct SocketStats {
	rx: usize,
	tx: usize,
}

#[derive(Copy, Clone)]
struct MsgPackSerializer {}

impl Serialization for MsgPackSerializer {
	fn serialize(&self, value: impl serde::Serialize) -> Vec<u8> {
		rmp_serde::to_vec_named(&value).expect("expected to serialize")
	}

	fn deserialize<P: serde::de::DeserializeOwned>(&self, b: &[u8]) -> Result<P, AnyError> {
		rmp_serde::from_slice(b).map_err(|e| InvalidRpcDataError(e.to_string()).into())
	}
}

#[allow(clippy::too_many_arguments)] // necessary here
async fn process_socket(
	mut exit_barrier: Barrier<()>,
	readhalf: impl AsyncRead + Send + Unpin + 'static,
	mut writehalf: impl AsyncWrite + Unpin,
	log: log::Logger,
	server_tx: mpsc::Sender<ServerSignal>,
	launcher_paths: LauncherPaths,
	code_server_args: CodeServerArgs,
	port_forwarding: PortForwarding,
	platform: Platform,
) -> SocketStats {
	let (socket_tx, mut socket_rx) = mpsc::channel(4);
	let rx_counter = Arc::new(AtomicUsize::new(0));
	let http_requests = Arc::new(std::sync::Mutex::new(HashMap::new()));
	let server_bridges = ServerMultiplexer::new();
	let (http_delegated, mut http_rx) = DelegatedSimpleHttp::new(log.clone());
	let mut rpc = RpcBuilder::new(MsgPackSerializer {}).methods(HandlerContext {
		did_update: Arc::new(AtomicBool::new(false)),
		socket_tx: socket_tx.clone(),
		log: log.clone(),
		launcher_paths,
		code_server_args,
		code_server: Arc::new(Mutex::new(None)),
		server_bridges: server_bridges.clone(),
		port_forwarding,
		platform,
		http: Arc::new(FallbackSimpleHttp::new(
			ReqwestSimpleHttp::new(),
			http_delegated,
		)),
		http_requests: http_requests.clone(),
	});

	rpc.register_sync("ping", |_: EmptyObject, _| Ok(EmptyObject {}));
	rpc.register_sync("gethostname", |_: EmptyObject, _| handle_get_hostname());
	rpc.register_async("serve", move |params: ServeParams, c| async move {
		handle_serve(c, params).await
	});
	rpc.register_async("update", |p: UpdateParams, c| async move {
		handle_update(&c.http, &c.log, &c.did_update, &p).await
	});
	rpc.register_sync("servermsg", |m: ServerMessageParams, c| {
		if let Err(e) = handle_server_message(&c.log, &c.server_bridges, m) {
			warning!(c.log, "error handling call: {:?}", e);
		}
		Ok(EmptyObject {})
	});
	rpc.register_sync("prune", |_: EmptyObject, c| handle_prune(&c.launcher_paths));
	rpc.register_async("callserverhttp", |p: CallServerHttpParams, c| async move {
		let code_server = c.code_server.lock().await.clone();
		handle_call_server_http(code_server, p).await
	});
	rpc.register_async("forward", |p: ForwardParams, c| async move {
		handle_forward(&c.log, &c.port_forwarding, p).await
	});
	rpc.register_async("unforward", |p: UnforwardParams, c| async move {
		handle_unforward(&c.log, &c.port_forwarding, p).await
	});
	rpc.register_async("acquire_cli", |p: AcquireCliParams, c| async move {
		handle_acquire_cli(&c.launcher_paths, &c.http, &c.log, p).await
	});
	rpc.register_duplex("spawn", 3, |mut streams, p: SpawnParams, c| async move {
		handle_spawn(
			&c.log,
			p,
			Some(streams.remove(0)),
			Some(streams.remove(0)),
			Some(streams.remove(0)),
		)
		.await
	});
	rpc.register_sync("httpheaders", |p: HttpHeadersParams, c| {
		if let Some(req) = c.http_requests.lock().unwrap().get(&p.req_id) {
			req.initial_response(p.status_code, p.headers);
		}
		Ok(EmptyObject {})
	});
	rpc.register_sync("unforward", move |p: HttpBodyParams, c| {
		let mut reqs = c.http_requests.lock().unwrap();
		if let Some(req) = reqs.get(&p.req_id) {
			if !p.segment.is_empty() {
				req.body(p.segment);
			}
			if p.complete {
				reqs.remove(&p.req_id);
			}
		}
		Ok(EmptyObject {})
	});

	{
		let log = log.clone();
		let rx_counter = rx_counter.clone();
		let socket_tx = socket_tx.clone();
		let exit_barrier = exit_barrier.clone();
		let rpc = rpc.build(log.clone());
		tokio::spawn(async move {
			send_version(&socket_tx).await;

			if let Err(e) =
				handle_socket_read(&log, readhalf, exit_barrier, &socket_tx, rx_counter, &rpc).await
			{
				debug!(log, "closing socket reader: {}", e);
				socket_tx
					.send(SocketSignal::CloseWith(CloseReason(format!("{}", e))))
					.await
					.ok();
			}

			let ctx = rpc.context();

			// The connection is now closed, asked to respawn if needed
			if ctx.did_update.load(Ordering::SeqCst) {
				server_tx.send(ServerSignal::Respawn).await.ok();
			}

			ctx.dispose().await;
		});
	}

	let mut tx_counter = 0;

	loop {
		tokio::select! {
			_ = exit_barrier.wait() => {
				writehalf.shutdown().await.ok();
				break;
			},
			Some(r) = http_rx.recv() => {
				let id = next_message_id();
				let serialized = rmp_serde::to_vec_named(&ToClientRequest {
					id: None,
					params: ClientRequestMethod::makehttpreq(HttpRequestParams {
						url: &r.url,
						method: r.method,
						req_id: id,
					}),
				})
				.unwrap();
				http_requests.lock().unwrap().insert(id, r);

				tx_counter += serialized.len();
				if let Err(e) = writehalf.write_all(&serialized).await {
					debug!(log, "Closing connection: {}", e);
					break;
				}
			}
			recv = socket_rx.recv() => match recv {
				None => break,
				Some(message) => match message {
					SocketSignal::Send(bytes) => {
						tx_counter += bytes.len();
						if let Err(e) = writehalf.write_all(&bytes).await {
							debug!(log, "Closing connection: {}", e);
							break;
						}
					}
					SocketSignal::CloseWith(reason) => {
						debug!(log, "Closing connection: {}", reason.0);
						break;
					}
				}
			}
		}
	}

	SocketStats {
		tx: tx_counter,
		rx: rx_counter.load(Ordering::Acquire),
	}
}

async fn send_version(tx: &mpsc::Sender<SocketSignal>) {
	tx.send(SocketSignal::from_message(&ToClientRequest {
		id: None,
		params: ClientRequestMethod::version(VersionParams::default()),
	}))
	.await
	.ok();
}
async fn handle_socket_read(
	_log: &log::Logger,
	readhalf: impl AsyncRead + Unpin,
	mut closer: Barrier<()>,
	socket_tx: &mpsc::Sender<SocketSignal>,
	rx_counter: Arc<AtomicUsize>,
	rpc: &RpcDispatcher<MsgPackSerializer, HandlerContext>,
) -> Result<(), std::io::Error> {
	let mut readhalf = BufReader::new(readhalf);
	let mut decoder = U32PrefixedCodec {};
	let mut decoder_buf = bytes::BytesMut::new();

	loop {
		let read_len = tokio::select! {
			r = readhalf.read_buf(&mut decoder_buf) => r,
			_ = closer.wait() => Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof")),
		}?;

		rx_counter.fetch_add(read_len, Ordering::Relaxed);

		while let Some(frame) = decoder.decode(&mut decoder_buf)? {
			match rpc.dispatch(&frame) {
				MaybeSync::Sync(Some(v)) => {
					if socket_tx.send(SocketSignal::Send(v)).await.is_err() {
						return Ok(());
					}
				}
				MaybeSync::Sync(None) => continue,
				MaybeSync::Future(fut) => {
					let socket_tx = socket_tx.clone();
					tokio::spawn(async move {
						if let Some(v) = fut.await {
							socket_tx.send(SocketSignal::Send(v)).await.ok();
						}
					});
				}
				MaybeSync::Stream((stream, fut)) => {
					if let Some(stream) = stream {
						rpc.register_stream(socket_tx.clone(), stream).await;
					}
					let socket_tx = socket_tx.clone();
					tokio::spawn(async move {
						if let Some(v) = fut.await {
							socket_tx.send(SocketSignal::Send(v)).await.ok();
						}
					});
				}
			}
		}
	}
}

#[derive(Clone)]
struct ServerOutputSink {
	tx: mpsc::Sender<SocketSignal>,
}

impl log::LogSink for ServerOutputSink {
	fn write_log(&self, level: log::Level, _prefix: &str, message: &str) {
		let s = SocketSignal::from_message(&ToClientRequest {
			id: None,
			params: ClientRequestMethod::serverlog(ServerLog {
				line: message,
				level: level.to_u8(),
			}),
		});

		self.tx.try_send(s).ok();
	}

	fn write_result(&self, _message: &str) {}
}

async fn handle_serve(
	c: Arc<HandlerContext>,
	params: ServeParams,
) -> Result<EmptyObject, AnyError> {
	// fill params.extensions into code_server_args.install_extensions
	let mut csa = c.code_server_args.clone();
	csa.install_extensions.extend(params.extensions.into_iter());

	let params_raw = ServerParamsRaw {
		commit_id: params.commit_id,
		quality: params.quality,
		code_server_args: csa,
		headless: true,
		platform: c.platform,
	};

	let resolved = if params.use_local_download {
		params_raw
			.resolve(&c.log, Arc::new(c.http.delegated()))
			.await
	} else {
		params_raw.resolve(&c.log, c.http.clone()).await
	}?;

	let mut server_ref = c.code_server.lock().await;
	let server = match &*server_ref {
		Some(o) => o.clone(),
		None => {
			let install_log = c.log.tee(ServerOutputSink {
				tx: c.socket_tx.clone(),
			});

			macro_rules! do_setup {
				($sb:expr) => {
					match $sb.get_running().await? {
						Some(AnyCodeServer::Socket(s)) => s,
						Some(_) => return Err(AnyError::from(MismatchedLaunchModeError())),
						None => {
							$sb.setup().await?;
							$sb.listen_on_default_socket().await?
						}
					}
				};
			}

			let server = if params.use_local_download {
				let sb = ServerBuilder::new(
					&install_log,
					&resolved,
					&c.launcher_paths,
					Arc::new(c.http.delegated()),
				);
				do_setup!(sb)
			} else {
				let sb =
					ServerBuilder::new(&install_log, &resolved, &c.launcher_paths, c.http.clone());
				do_setup!(sb)
			};

			server_ref.replace(server.clone());
			server
		}
	};

	attach_server_bridge(
		&c.log,
		server,
		c.socket_tx.clone(),
		c.server_bridges.clone(),
		params.socket_id,
		params.compress,
	)
	.await?;
	Ok(EmptyObject {})
}

async fn attach_server_bridge(
	log: &log::Logger,
	code_server: SocketCodeServer,
	socket_tx: mpsc::Sender<SocketSignal>,
	multiplexer: ServerMultiplexer,
	socket_id: u16,
	compress: bool,
) -> Result<u16, AnyError> {
	let (server_messages, decoder) = if compress {
		(
			ServerMessageSink::new_compressed(
				multiplexer.clone(),
				socket_id,
				ServerMessageDestination::Channel(socket_tx),
			),
			ClientMessageDecoder::new_compressed(),
		)
	} else {
		(
			ServerMessageSink::new_plain(
				multiplexer.clone(),
				socket_id,
				ServerMessageDestination::Channel(socket_tx),
			),
			ClientMessageDecoder::new_plain(),
		)
	};

	let attached_fut = ServerBridge::new(&code_server.socket, server_messages, decoder).await;
	match attached_fut {
		Ok(a) => {
			multiplexer.register(socket_id, a);
			trace!(log, "Attached to server");
			Ok(socket_id)
		}
		Err(e) => Err(e),
	}
}

/// Handle an incoming server message. This is synchronous and uses a 'write loop'
/// to ensure message order is preserved exactly, which is necessary for compression.
fn handle_server_message(
	log: &log::Logger,
	multiplexer: &ServerMultiplexer,
	params: ServerMessageParams,
) -> Result<EmptyObject, AnyError> {
	if multiplexer.write_message(log, params.i, params.body) {
		Ok(EmptyObject {})
	} else {
		Err(AnyError::from(NoAttachedServerError()))
	}
}

fn handle_prune(paths: &LauncherPaths) -> Result<Vec<String>, AnyError> {
	prune_stopped_servers(paths).map(|v| {
		v.iter()
			.map(|p| p.server_dir.display().to_string())
			.collect()
	})
}

async fn handle_update(
	http: &Arc<FallbackSimpleHttp>,
	log: &log::Logger,
	did_update: &AtomicBool,
	params: &UpdateParams,
) -> Result<UpdateResult, AnyError> {
	if matches!(is_integrated_cli(), Ok(true)) || did_update.load(Ordering::SeqCst) {
		return Ok(UpdateResult {
			up_to_date: true,
			did_update: false,
		});
	}

	let update_service = UpdateService::new(log.clone(), http.clone());
	let updater = SelfUpdate::new(&update_service)?;
	let latest_release = updater.get_current_release().await?;
	let up_to_date = updater.is_up_to_date_with(&latest_release);

	if !params.do_update || up_to_date {
		return Ok(UpdateResult {
			up_to_date,
			did_update: false,
		});
	}

	if did_update
		.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
		.is_err()
	{
		return Ok(UpdateResult {
			up_to_date: true,
			did_update: true, // well, another thread did, but same difference...
		});
	}

	info!(log, "Updating CLI to {}", latest_release);

	updater
		.do_update(&latest_release, SilentCopyProgress())
		.await?;

	Ok(UpdateResult {
		up_to_date: true,
		did_update: true,
	})
}

fn handle_get_hostname() -> Result<GetHostnameResponse, AnyError> {
	Ok(GetHostnameResponse {
		value: gethostname::gethostname().to_string_lossy().into_owned(),
	})
}

async fn handle_forward(
	log: &log::Logger,
	port_forwarding: &PortForwarding,
	params: ForwardParams,
) -> Result<ForwardResult, AnyError> {
	info!(log, "Forwarding port {}", params.port);
	let uri = port_forwarding.forward(params.port).await?;
	Ok(ForwardResult { uri })
}

async fn handle_unforward(
	log: &log::Logger,
	port_forwarding: &PortForwarding,
	params: UnforwardParams,
) -> Result<EmptyObject, AnyError> {
	info!(log, "Unforwarding port {}", params.port);
	port_forwarding.unforward(params.port).await?;
	Ok(EmptyObject {})
}

async fn handle_call_server_http(
	code_server: Option<SocketCodeServer>,
	params: CallServerHttpParams,
) -> Result<CallServerHttpResult, AnyError> {
	use hyper::{body, client::conn::Builder, Body, Request};

	// We use Hyper directly here since reqwest doesn't support sockets/pipes.
	// See https://github.com/seanmonstar/reqwest/issues/39

	let socket = match &code_server {
		Some(cs) => &cs.socket,
		None => return Err(AnyError::from(NoAttachedServerError())),
	};

	let rw = get_socket_rw_stream(socket).await?;

	let (mut request_sender, connection) = Builder::new()
		.handshake(rw)
		.await
		.map_err(|e| wrap(e, "error establishing connection"))?;

	// start the connection processing; it's shut down when the sender is dropped
	tokio::spawn(connection);

	let mut request_builder = Request::builder()
		.method::<&str>(params.method.as_ref())
		.uri(format!("http://127.0.0.1{}", params.path))
		.header("Host", "127.0.0.1");

	for (k, v) in params.headers {
		request_builder = request_builder.header(k, v);
	}
	let request = request_builder
		.body(Body::from(params.body.unwrap_or_default()))
		.map_err(|e| wrap(e, "invalid request"))?;

	let response = request_sender
		.send_request(request)
		.await
		.map_err(|e| wrap(e, "error sending request"))?;

	Ok(CallServerHttpResult {
		status: response.status().as_u16(),
		headers: response
			.headers()
			.into_iter()
			.map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
			.collect(),
		body: body::to_bytes(response)
			.await
			.map_err(|e| wrap(e, "error reading response body"))?
			.to_vec(),
	})
}

async fn handle_acquire_cli(
	paths: &LauncherPaths,
	http: &Arc<FallbackSimpleHttp>,
	log: &log::Logger,
	params: AcquireCliParams,
) -> Result<SpawnResult, AnyError> {
	let update_service = UpdateService::new(log.clone(), http.clone());

	let release = match params.commit_id {
		Some(commit) => Release {
			name: format!("{} CLI", PRODUCT_NAME_LONG),
			commit,
			platform: params.platform,
			quality: params.quality,
			target: TargetKind::Cli,
		},
		None => {
			update_service
				.get_latest_commit(params.platform, TargetKind::Cli, params.quality)
				.await?
		}
	};

	let cli = download_cli_into_cache(&paths.cli_cache, &release, &update_service).await?;
	let file = tokio::fs::File::open(cli)
		.await
		.map_err(|e| wrap(e, "error opening cli file"))?;

	handle_spawn::<_, DuplexStream>(log, params.spawn, Some(file), None, None).await
}

async fn handle_spawn<Stdin, StdoutAndErr>(
	log: &log::Logger,
	params: SpawnParams,
	stdin: Option<Stdin>,
	stdout: Option<StdoutAndErr>,
	stderr: Option<StdoutAndErr>,
) -> Result<SpawnResult, AnyError>
where
	Stdin: AsyncRead + Unpin + Send,
	StdoutAndErr: AsyncWrite + Unpin + Send,
{
	debug!(
		log,
		"requested to spawn {} with args {:?}", params.command, params.args
	);

	macro_rules! pipe_if_some {
		($e: expr) => {
			if $e.is_some() {
				Stdio::piped()
			} else {
				Stdio::null()
			}
		};
	}

	let mut p = tokio::process::Command::new(&params.command)
		.args(&params.args)
		.envs(&params.env)
		.stdin(pipe_if_some!(stdin))
		.stdout(pipe_if_some!(stdout))
		.stderr(pipe_if_some!(stderr))
		.spawn()
		.map_err(CodeError::ProcessSpawnFailed)?;

	let futs = FuturesUnordered::new();
	if let (Some(mut a), Some(mut b)) = (p.stdout.take(), stdout) {
		futs.push(async move { tokio::io::copy(&mut a, &mut b).await }.boxed());
	}
	if let (Some(mut a), Some(mut b)) = (p.stderr.take(), stderr) {
		futs.push(async move { tokio::io::copy(&mut a, &mut b).await }.boxed());
	}
	if let (Some(mut b), Some(mut a)) = (p.stdin.take(), stdin) {
		futs.push(async move { tokio::io::copy(&mut a, &mut b).await }.boxed());
	}

	let closed = p.wait();
	pin!(closed);

	let r = tokio::select! {
		_ = futures::future::join_all(futs) => closed.await,
		r = &mut closed => r
	};

	let r = match r {
		Ok(e) => SpawnResult {
			message: e.to_string(),
			exit_code: e.code().unwrap_or(-1),
		},
		Err(e) => SpawnResult {
			message: e.to_string(),
			exit_code: -1,
		},
	};

	debug!(
		log,
		"spawned command {} exited with code {}", params.command, r.exit_code
	);

	Ok(r)
}
