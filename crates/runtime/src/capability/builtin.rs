use super::logging::logging;
use super::{format_opt, messaging};

use core::fmt::Debug;
use core::future::Future;
use core::pin::Pin;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{instrument, trace};

#[derive(Clone, Default)]
pub struct Handler {
    blobstore: Option<Arc<dyn Blobstore + Sync + Send>>,
    bus: Option<Arc<dyn Bus + Sync + Send>>,
    keyvalue_readwrite: Option<Arc<dyn KeyValueReadWrite + Sync + Send>>,
    logging: Option<Arc<dyn Logging + Sync + Send>>,
    incoming_http: Option<Arc<dyn IncomingHttp + Sync + Send>>,
    messaging: Option<Arc<dyn Messaging + Sync + Send>>,
}

impl Debug for Handler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handler")
            .field("blobstore", &format_opt(&self.blobstore))
            .field("bus", &format_opt(&self.bus))
            .field("incoming_http", &format_opt(&self.incoming_http))
            .field("keyvalue_readwrite", &format_opt(&self.keyvalue_readwrite))
            .field("logging", &format_opt(&self.logging))
            .field("messaging", &format_opt(&self.messaging))
            .finish()
    }
}

impl Handler {
    /// Replace [`Blobstore`] handler returning the old one, if such was set
    pub fn replace_blobstore(
        &mut self,
        blobstore: Arc<dyn Blobstore + Send + Sync>,
    ) -> Option<Arc<dyn Blobstore + Send + Sync>> {
        self.blobstore.replace(blobstore)
    }

    /// Replace [`Bus`] handler returning the old one, if such was set
    pub fn replace_bus(
        &mut self,
        bus: Arc<dyn Bus + Send + Sync>,
    ) -> Option<Arc<dyn Bus + Send + Sync>> {
        self.bus.replace(bus)
    }

    /// Replace [`IncomingHttp`] handler returning the old one, if such was set
    pub fn replace_incoming_http(
        &mut self,
        incoming_http: Arc<dyn IncomingHttp + Send + Sync>,
    ) -> Option<Arc<dyn IncomingHttp + Send + Sync>> {
        self.incoming_http.replace(incoming_http)
    }

    /// Replace [`KeyValueReadWrite`] handler returning the old one, if such was set
    pub fn replace_keyvalue_readwrite(
        &mut self,
        keyvalue_readwrite: Arc<dyn KeyValueReadWrite + Send + Sync>,
    ) -> Option<Arc<dyn KeyValueReadWrite + Send + Sync>> {
        self.keyvalue_readwrite.replace(keyvalue_readwrite)
    }

    /// Replace [`Logging`] handler returning the old one, if such was set
    pub fn replace_logging(
        &mut self,
        logging: Arc<dyn Logging + Send + Sync>,
    ) -> Option<Arc<dyn Logging + Send + Sync>> {
        self.logging.replace(logging)
    }

    /// Replace [`Messaging`] handler returning the old one, if such was set
    pub fn replace_messaging(
        &mut self,
        messaging: Arc<dyn Messaging + Send + Sync>,
    ) -> Option<Arc<dyn Messaging + Send + Sync>> {
        self.messaging.replace(messaging)
    }
}

#[async_trait]
/// `wasmcloud:bus/host` implementation
pub trait Bus {
    /// Handle `wasmcloud:bus/host.call`
    async fn call(
        &self,
        operation: String,
    ) -> anyhow::Result<(
        Pin<Box<dyn Future<Output = Result<(), String>> + Send>>,
        Box<dyn AsyncWrite + Sync + Send + Unpin>,
        Box<dyn AsyncRead + Sync + Send + Unpin>,
    )>;

    /// Handle `wasmcloud:bus/host.call` without streaming and with no response
    async fn call_oneshot(
        &self,
        operation: String,
        request: Vec<u8>,
    ) -> anyhow::Result<Result<(), String>> {
        let (res, mut input, mut output) = self
            .call(operation)
            .await
            .context("failed to process call")?;
        input
            .write_all(&request)
            .await
            .context("failed to write request")?;
        let n = output
            .read_buf(&mut [0u8].as_mut_slice())
            .await
            .context("failed to read output")?;
        ensure!(n == 0, "unexpected output received");
        Ok(res.await)
    }

    /// Handle `wasmcloud:bus/host.call` without streaming
    async fn call_oneshot_with_response(
        &self,
        operation: String,
        request: Vec<u8>,
        response: &mut Vec<u8>,
    ) -> anyhow::Result<Result<usize, String>> {
        let (res, mut input, mut output) = self
            .call(operation)
            .await
            .context("failed to process call")?;
        input
            .write_all(&request)
            .await
            .context("failed to write request")?;
        let n = output
            .read_to_end(response)
            .await
            .context("failed to read output")?;
        Ok(res.await.map(|()| n))
    }
}

#[async_trait]
/// `wasi:http/incoming-handler` implementation
pub trait IncomingHttp {
    /// Handle `wasi:http/incoming-handler`
    async fn handle(
        &self,
        request: http::Request<Box<dyn AsyncRead + Sync + Send + Unpin>>,
    ) -> anyhow::Result<http::Response<Box<dyn AsyncRead + Sync + Send + Unpin>>>;
}

#[async_trait]
/// `wasi:blobstore/consumer` implementation
pub trait Blobstore {
    // TODO: Implement
}

#[async_trait]
/// `wasi:logging/logging` implementation
pub trait Logging {
    /// Handle `wasi:logging/logging.log`
    async fn log(
        &self,
        level: logging::Level,
        context: String,
        message: String,
    ) -> anyhow::Result<()>;
}

#[async_trait]
/// `wasi:keyvalue/readwrite` implementation
pub trait KeyValueReadWrite {
    /// Handle `wasi:keyvalue/readwrite.get`
    async fn get(
        &self,
        bucket: &str,
        key: String,
    ) -> anyhow::Result<(Box<dyn AsyncRead + Sync + Send + Unpin>, u64)>;

    /// Handle `wasi:keyvalue/readwrite.set`
    async fn set(
        &self,
        bucket: &str,
        key: String,
        value: Box<dyn AsyncRead + Sync + Send + Unpin>,
    ) -> anyhow::Result<()>;

    /// Handle `wasi:keyvalue/readwrite.delete`
    async fn delete(&self, bucket: &str, key: String) -> anyhow::Result<()>;

    /// Handle `wasi:keyvalue/readwrite.exists`
    async fn exists(&self, bucket: &str, key: String) -> anyhow::Result<bool>;
}

#[async_trait]
/// `wasmcloud:messaging/consumer` implementation
pub trait Messaging {
    /// Handle `wasmcloud:messaging/consumer.request`
    async fn request(
        &self,
        subject: String,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> anyhow::Result<messaging::types::BrokerMessage>;

    /// Handle `wasmcloud:messaging/consumer.request_multi`
    async fn request_multi(
        &self,
        subject: String,
        body: Option<Vec<u8>>,
        timeout: Duration,
        max_results: u32,
    ) -> anyhow::Result<Vec<messaging::types::BrokerMessage>>;

    /// Handle `wasmcloud:messaging/consumer.publish`
    async fn publish(&self, msg: messaging::types::BrokerMessage) -> anyhow::Result<()>;
}

#[async_trait]
impl Bus for Handler {
    #[instrument]
    async fn call(
        &self,
        operation: String,
    ) -> anyhow::Result<(
        Pin<Box<dyn Future<Output = Result<(), String>> + Send>>,
        Box<dyn AsyncWrite + Sync + Send + Unpin>,
        Box<dyn AsyncRead + Sync + Send + Unpin>,
    )> {
        if let Some(ref bus) = self.bus {
            trace!("call `Bus` handler");
            bus.call(operation).await
        } else {
            bail!("host cannot handle `{operation}`")
        }
    }
}

#[async_trait]
impl Logging for Handler {
    #[instrument]
    async fn log(
        &self,
        level: logging::Level,
        context: String,
        message: String,
    ) -> anyhow::Result<()> {
        if let Some(ref logging) = self.logging {
            trace!("call `Logging` handler");
            logging.log(level, context, message).await
        } else {
            // discard all log invocations by default
            Ok(())
        }
    }
}

#[async_trait]
impl KeyValueReadWrite for Handler {
    #[instrument]
    async fn get(
        &self,
        bucket: &str,
        key: String,
    ) -> anyhow::Result<(Box<dyn AsyncRead + Sync + Send + Unpin>, u64)> {
        trace!("call `KeyValueReadWrite` handler");
        self.keyvalue_readwrite
            .as_ref()
            .context("cannot handle `wasi:keyvalue/readwrite.get`")?
            .get(bucket, key)
            .await
    }

    #[instrument(skip(value))]
    async fn set(
        &self,
        bucket: &str,
        key: String,
        value: Box<dyn AsyncRead + Sync + Send + Unpin>,
    ) -> anyhow::Result<()> {
        trace!("call `KeyValueReadWrite` handler");
        self.keyvalue_readwrite
            .as_ref()
            .context("cannot handle `wasi:keyvalue/readwrite.set`")?
            .set(bucket, key, value)
            .await
    }

    #[instrument]
    async fn delete(&self, bucket: &str, key: String) -> anyhow::Result<()> {
        trace!("call `KeyValueReadWrite` handler");
        self.keyvalue_readwrite
            .as_ref()
            .context("cannot handle `wasi:keyvalue/readwrite.delete`")?
            .delete(bucket, key)
            .await
    }

    #[instrument]
    async fn exists(&self, bucket: &str, key: String) -> anyhow::Result<bool> {
        trace!("call `KeyValueReadWrite` handler");
        self.keyvalue_readwrite
            .as_ref()
            .context("cannot handle `wasi:keyvalue/readwrite.exists`")?
            .exists(bucket, key)
            .await
    }
}

#[async_trait]
impl IncomingHttp for Handler {
    #[instrument(skip(request))]
    async fn handle(
        &self,
        request: http::Request<Box<dyn AsyncRead + Sync + Send + Unpin>>,
    ) -> anyhow::Result<http::Response<Box<dyn AsyncRead + Sync + Send + Unpin>>> {
        trace!("call `IncomingHttp` handler");
        self.incoming_http
            .as_ref()
            .context("cannot handle `wasi:http/incoming-handler.handle`")?
            .handle(request)
            .await
    }
}

#[async_trait]
impl Messaging for Handler {
    #[instrument(skip(body))]
    async fn request(
        &self,
        subject: String,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> anyhow::Result<messaging::types::BrokerMessage> {
        trace!("call `Messaging` handler");
        self.messaging
            .as_ref()
            .context("cannot handle `wasmcloud:messaging/consumer.request`")?
            .request(subject, body, timeout)
            .await
    }

    #[instrument(skip(body))]
    async fn request_multi(
        &self,
        subject: String,
        body: Option<Vec<u8>>,
        timeout: Duration,
        max_results: u32,
    ) -> anyhow::Result<Vec<messaging::types::BrokerMessage>> {
        trace!("call `Messaging` handler");
        self.messaging
            .as_ref()
            .context("cannot handle `wasmcloud:messaging/consumer.request_multi`")?
            .request_multi(subject, body, timeout, max_results)
            .await
    }

    #[instrument(skip(msg))]
    async fn publish(&self, msg: messaging::types::BrokerMessage) -> anyhow::Result<()> {
        trace!("call `Messaging` handler");
        self.messaging
            .as_ref()
            .context("cannot handle `wasmcloud:messaging/consumer.publish`")?
            .publish(msg)
            .await
    }
}

/// A [Handler] builder used to configure it
#[derive(Clone, Default)]
pub(crate) struct HandlerBuilder {
    /// [`Blobstore`] handler
    pub blobstore: Option<Arc<dyn Blobstore + Sync + Send>>,
    /// [`Bus`] handler
    pub bus: Option<Arc<dyn Bus + Sync + Send>>,
    /// [`IncomingHttp`] handler
    pub incoming_http: Option<Arc<dyn IncomingHttp + Sync + Send>>,
    /// [`KeyValueReadWrite`] handler
    pub keyvalue_readwrite: Option<Arc<dyn KeyValueReadWrite + Sync + Send>>,
    /// [`Logging`] handler
    pub logging: Option<Arc<dyn Logging + Sync + Send>>,
    /// [`Messaging`] handler
    pub messaging: Option<Arc<dyn Messaging + Sync + Send>>,
}

impl HandlerBuilder {
    /// Set [`Blobstore`] handler
    pub fn blobstore(self, blobstore: Arc<impl Blobstore + Sync + Send + 'static>) -> Self {
        Self {
            blobstore: Some(blobstore),
            ..self
        }
    }

    /// Set [`Bus`] handler
    pub fn bus(self, bus: Arc<impl Bus + Sync + Send + 'static>) -> Self {
        Self {
            bus: Some(bus),
            ..self
        }
    }

    /// Set [`IncomingHttp`] handler
    pub fn incoming_http(
        self,
        incoming_http: Arc<impl IncomingHttp + Sync + Send + 'static>,
    ) -> Self {
        Self {
            incoming_http: Some(incoming_http),
            ..self
        }
    }

    /// Set [`KeyValueReadWrite`] handler
    pub fn keyvalue_readwrite(
        self,
        keyvalue_readwrite: Arc<impl KeyValueReadWrite + Sync + Send + 'static>,
    ) -> Self {
        Self {
            keyvalue_readwrite: Some(keyvalue_readwrite),
            ..self
        }
    }

    /// Set [`Logging`] handler
    pub fn logging(self, logging: Arc<impl Logging + Sync + Send + 'static>) -> Self {
        Self {
            logging: Some(logging),
            ..self
        }
    }

    /// Set [`Messaging`] handler
    pub fn messaging(self, messaging: Arc<impl Messaging + Sync + Send + 'static>) -> Self {
        Self {
            messaging: Some(messaging),
            ..self
        }
    }
}

impl Debug for HandlerBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandlerBuilder")
            .field("blobstore", &format_opt(&self.blobstore))
            .field("bus", &format_opt(&self.bus))
            .field("incoming_http", &format_opt(&self.incoming_http))
            .field("keyvalue_readwrite", &format_opt(&self.keyvalue_readwrite))
            .field("logging", &format_opt(&self.logging))
            .field("messaging", &format_opt(&self.messaging))
            .finish()
    }
}

impl From<Handler> for HandlerBuilder {
    fn from(
        Handler {
            blobstore,
            bus,
            incoming_http,
            keyvalue_readwrite,
            logging,
            messaging,
        }: Handler,
    ) -> Self {
        Self {
            blobstore,
            bus,
            incoming_http,
            keyvalue_readwrite,
            logging,
            messaging,
        }
    }
}

impl From<HandlerBuilder> for Handler {
    fn from(
        HandlerBuilder {
            blobstore,
            bus,
            incoming_http,
            keyvalue_readwrite,
            logging,
            messaging,
        }: HandlerBuilder,
    ) -> Self {
        Self {
            blobstore,
            bus,
            keyvalue_readwrite,
            logging,
            incoming_http,
            messaging,
        }
    }
}
