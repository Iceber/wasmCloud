mod component;
mod module;

pub use component::{
    Component, GuestInstance as ComponentGuestInstance, Instance as ComponentInstance,
    InterfaceInstance as ComponentInterfaceInstance,
};
pub use module::{
    Config as ModuleConfig, GuestInstance as ModuleGuestInstance, Instance as ModuleInstance,
    Module,
};

use crate::capability::logging::logging;
use crate::capability::{Bus, IncomingHttp, KeyValueReadWrite, Logging, Messaging};
use crate::Runtime;

use core::fmt::Debug;
use core::num::NonZeroUsize;
use core::ops::{Deref, DerefMut};

use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::runtime::Handle;
use tokio::sync::RwLock;
use tokio::task;
use tracing::instrument;
use wascap::jwt;
use wascap::wasm::extract_claims;

/// Actor instance configuration
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// Whether actors are required to be signed to be executed
    pub require_signature: bool,
}

/// Extracts and validates claims contained within `WebAssembly` binary, if such are found
fn claims(wasm: impl AsRef<[u8]>) -> Result<Option<jwt::Claims<jwt::Actor>>> {
    let Some(claims) = extract_claims(wasm).context("failed to extract module claims")? else {
        return Ok(None)
    };
    let v = jwt::validate_token::<jwt::Actor>(&claims.jwt)
        .context("failed to validate module token")?;
    ensure!(!v.expired, "token expired at `{}`", v.expires_human);
    ensure!(
        !v.cannot_use_yet,
        "token cannot be used before `{}`",
        v.not_before_human
    );
    ensure!(v.signature_valid, "signature is not valid");
    Ok(Some(claims.claims))
}

/// A pre-loaded wasmCloud actor, which is either a module or a component
#[derive(Clone, Debug)]
pub enum Actor {
    /// WebAssembly module containing an actor
    Module(Module),
    /// WebAssembly component containing an actor
    Component(Component),
}

impl Actor {
    /// Compiles WebAssembly binary using [Runtime].
    ///
    /// # Errors
    ///
    /// Fails if [Component::new] or [Module::new] fails
    #[instrument(skip(wasm))]
    pub fn new(rt: &Runtime, wasm: impl AsRef<[u8]>) -> Result<Self> {
        let wasm = wasm.as_ref();
        // TODO: Optimize parsing, add functionality to `wascap` to parse from a custom section
        // directly
        match wasmparser::Parser::new(0).parse_all(wasm).next() {
            Some(Ok(wasmparser::Payload::Version {
                encoding: wasmparser::Encoding::Component,
                ..
            })) => Component::new(rt, wasm).map(Self::Component),
            // fallback to module type
            _ => Module::new(rt, wasm).map(Self::Module),
        }
    }

    /// Reads the WebAssembly binary asynchronously and calls [Actor::new].
    ///
    /// # Errors
    ///
    /// Fails if either reading `wasm` fails or [Self::new] fails
    #[instrument(skip(wasm))]
    pub async fn read(rt: &Runtime, mut wasm: impl AsyncRead + Unpin) -> Result<Self> {
        let mut buf = Vec::new();
        wasm.read_to_end(&mut buf)
            .await
            .context("failed to read Wasm")?;
        Self::new(rt, buf)
    }

    /// Reads the WebAssembly binary synchronously and calls [Actor::new].
    ///
    /// # Errors
    ///
    /// Fails if either reading `wasm` fails or [Self::new] fails
    #[instrument(skip(wasm))]
    pub fn read_sync(rt: &Runtime, mut wasm: impl std::io::Read) -> Result<Self> {
        let mut buf = Vec::new();
        wasm.read_to_end(&mut buf).context("failed to read Wasm")?;
        Self::new(rt, buf)
    }

    /// [Claims](jwt::Claims) associated with this [Actor].
    #[instrument]
    pub fn claims(&self) -> Option<&jwt::Claims<jwt::Actor>> {
        match self {
            Self::Module(module) => module.claims(),
            Self::Component(component) => component.claims(),
        }
    }

    /// Like [Self::instantiate], but moves the [Actor].
    #[instrument]
    pub async fn into_instance(self) -> anyhow::Result<Instance> {
        match self {
            Self::Module(module) => module.into_instance().await.map(Instance::Module),
            Self::Component(component) => component.into_instance().map(Instance::Component),
        }
    }

    /// Like [Self::instantiate], but moves the [Actor] and returns associated [jwt::Claims].
    #[instrument]
    pub async fn into_instance_claims(
        self,
    ) -> anyhow::Result<(Instance, Option<jwt::Claims<jwt::Actor>>)> {
        match self {
            Actor::Module(module) => {
                let (module, claims) = module.into_instance_claims().await?;
                Ok((Instance::Module(module), claims))
            }
            Actor::Component(component) => {
                let (component, claims) = component.into_instance_claims()?;
                Ok((Instance::Component(component), claims))
            }
        }
    }

    /// Instantiate the actor.
    ///
    /// # Errors
    ///
    /// Fails if instantiation of the underlying module or component fails
    #[instrument]
    pub async fn instantiate(&self) -> anyhow::Result<Instance> {
        match self {
            Self::Module(module) => module.instantiate().await.map(Instance::Module),
            Self::Component(component) => component.instantiate().map(Instance::Component),
        }
    }

    /// Instantiate the actor and invoke an operation on it.
    ///
    /// # Errors
    ///
    /// Fails if [`Instance::call`] fails
    #[instrument(skip_all)]
    pub async fn call(
        &self,
        operation: impl AsRef<str>,
        request: impl AsyncRead + Send + Sync + Unpin + 'static,
        response: impl AsyncWrite + Send + Sync + Unpin + 'static,
    ) -> anyhow::Result<Result<(), String>> {
        self.instantiate()
            .await
            .context("failed to instantiate actor")?
            .call(operation, request, response)
            .await
    }

    /// Instantiates and returns a [`GuestInstance`] if exported by the [`Instance`].
    ///
    /// # Errors
    ///
    /// Fails if either instantiation fails or no guest bindings are exported by the [`Instance`]
    pub async fn as_guest(&self) -> anyhow::Result<GuestInstance> {
        self.instantiate()
            .await
            .context("failed to instantiate actor")?
            .into_guest()
            .await
    }

    /// Instantiates and returns a [`IncomingHttpInstance`] if exported by the [`Instance`].
    ///
    /// # Errors
    ///
    /// Fails if either instantiation fails or no incoming HTTP bindings are exported by the [`Instance`]
    pub async fn as_incoming_http(&self) -> anyhow::Result<IncomingHttpInstance> {
        self.instantiate()
            .await
            .context("failed to instantiate actor")?
            .into_incoming_http()
            .await
    }

    /// Instantiates and returns a [`LoggingInstance`] if exported by the [`Instance`].
    ///
    /// # Errors
    ///
    /// Fails if either instantiation fails or no logging bindings are exported by the [`Instance`]
    pub async fn as_logging(&self) -> anyhow::Result<LoggingInstance> {
        self.instantiate()
            .await
            .context("failed to instantiate actor")?
            .into_logging()
            .await
    }
}

/// A pre-loaded, configured wasmCloud actor instance, which is either a module or a component
#[derive(Debug)]
pub enum Instance {
    /// WebAssembly module containing an actor
    Module(ModuleInstance),
    /// WebAssembly component containing an actor
    Component(ComponentInstance),
}

/// A pre-loaded, configured guest instance, which is either a module or a component
#[derive(Clone)]
pub enum GuestInstance {
    /// WebAssembly module containing an actor
    Module(ModuleGuestInstance),
    /// WebAssembly component containing an actor
    Component(ComponentGuestInstance),
}

/// A pre-loaded, configured [Logging] instance, which is either a module or a component
pub enum LoggingInstance {
    /// WebAssembly module containing an actor
    Module(ModuleGuestInstance),
    /// WebAssembly component containing an actor
    Component(ComponentInterfaceInstance<component::logging_bindings::Logging>),
}

/// A pre-loaded, configured [`IncomingHttp`] instance, which is either a module or a component
pub enum IncomingHttpInstance {
    /// WebAssembly module containing an actor
    Module(ModuleGuestInstance),
    /// WebAssembly component containing an actor
    Component(ComponentInterfaceInstance<component::incoming_http_bindings::IncomingHttp>),
}

impl GuestInstance {
    /// Invoke an operation on a [GuestInstance] producing a response
    ///
    /// # Errors
    ///
    /// Outermost error represents a failure in calling the actor, innermost - the
    /// application-layer error originating from within the actor itself
    #[instrument(skip_all)]
    pub async fn call(
        &self,
        operation: impl AsRef<str>,
        request: impl AsyncRead + Send + Sync + Unpin + 'static,
        response: impl AsyncWrite + Send + Sync + Unpin + 'static,
    ) -> anyhow::Result<Result<(), String>> {
        match self {
            Self::Module(module) => module
                .call(operation, request, response)
                .await
                .context("failed to call module"),
            Self::Component(component) => component
                .call(operation, request, response)
                .await
                .context("failed to call component"),
        }
    }
}

#[async_trait]
impl Logging for LoggingInstance {
    async fn log(
        &self,
        level: logging::Level,
        context: String,
        message: String,
    ) -> anyhow::Result<()> {
        match self {
            Self::Module(module) => module.log(level, context, message),
            Self::Component(component) => component.log(level, context, message),
        }
        .await
    }
}

#[async_trait]
impl IncomingHttp for IncomingHttpInstance {
    async fn handle(
        &self,
        request: http::Request<Box<dyn AsyncRead + Sync + Send + Unpin>>,
    ) -> anyhow::Result<http::Response<Box<dyn AsyncRead + Sync + Send + Unpin>>> {
        match self {
            Self::Module(module) => module.handle(request),
            Self::Component(component) => component.handle(request),
        }
        .await
    }
}

impl Instance {
    /// Reset [`Instance`] state to defaults
    pub async fn reset(&mut self, rt: &Runtime) {
        match self {
            Self::Module(module) => module.reset(rt),
            Self::Component(component) => component.reset(rt).await,
        }
    }

    /// Set [`Bus`] handler for this [Instance].
    pub fn bus(&mut self, bus: Arc<dyn Bus + Send + Sync>) -> &mut Self {
        match self {
            Self::Module(module) => {
                module.bus(bus);
            }
            Self::Component(component) => {
                component.bus(bus);
            }
        }
        self
    }

    /// Set [`IncomingHttp`] handler for this [Instance].
    pub fn incoming_http(
        &mut self,
        incoming_http: Arc<dyn IncomingHttp + Send + Sync>,
    ) -> &mut Self {
        match self {
            Self::Module(module) => {
                module.incoming_http(incoming_http);
            }
            Self::Component(component) => {
                component.incoming_http(incoming_http);
            }
        }
        self
    }

    /// Set [`KeyValueReadWrite`] handler for this [Instance].
    pub fn keyvalue_readwrite(
        &mut self,
        keyvalue_readwrite: Arc<dyn KeyValueReadWrite + Send + Sync>,
    ) -> &mut Self {
        match self {
            Self::Module(module) => {
                module.keyvalue_readwrite(keyvalue_readwrite);
            }
            Self::Component(component) => {
                component.keyvalue_readwrite(keyvalue_readwrite);
            }
        }
        self
    }

    /// Set [`Logging`] handler for this [Instance].
    pub fn logging(&mut self, logging: Arc<dyn Logging + Send + Sync>) -> &mut Self {
        match self {
            Self::Module(module) => {
                module.logging(logging);
            }
            Self::Component(component) => {
                component.logging(logging);
            }
        }
        self
    }

    /// Set [`Messaging`] handler for this [Instance].
    pub fn messaging(&mut self, messaging: Arc<dyn Messaging + Send + Sync>) -> &mut Self {
        match self {
            Self::Module(module) => {
                module.messaging(messaging);
            }
            Self::Component(component) => {
                component.messaging(messaging);
            }
        }
        self
    }

    /// Set actor stderr stream. If another stderr was set, it is replaced and the old one is flushed and shut down if supported by underlying actor implementation.
    ///
    /// # Errors
    ///
    /// Fails if flushing and shutting down old stream fails
    pub async fn stderr(
        &mut self,
        stderr: impl AsyncWrite + Send + Sync + Unpin + 'static,
    ) -> anyhow::Result<&mut Self> {
        match self {
            Self::Module(module) => {
                module.stderr(stderr);
            }
            Self::Component(component) => {
                component.stderr(stderr).await?;
            }
        }
        Ok(self)
    }

    /// Invoke an operation on an [Instance] producing a response
    ///
    /// # Errors
    ///
    /// Outermost error represents a failure in calling the actor, innermost - the
    /// application-layer error originating from within the actor itself
    #[instrument(skip_all)]
    pub async fn call(
        &mut self,
        operation: impl AsRef<str>,
        request: impl AsyncRead + Send + Sync + Unpin + 'static,
        response: impl AsyncWrite + Send + Sync + Unpin + 'static,
    ) -> anyhow::Result<Result<(), String>> {
        match self {
            Self::Module(module) => module
                .call(operation, request, response)
                .await
                .context("failed to call module"),
            Self::Component(component) => component
                .call(operation, request, response)
                .await
                .context("failed to call component"),
        }
    }

    /// Instantiates and returns a [`GuestInstance`] if exported by the [`Instance`].
    ///
    /// # Errors
    ///
    /// Fails if no guest bindings are exported by the [`Instance`]
    pub async fn into_guest(self) -> anyhow::Result<GuestInstance> {
        match self {
            Self::Module(module) => Ok(GuestInstance::Module(ModuleGuestInstance::from(module))),
            Self::Component(component) => {
                component.into_guest().await.map(GuestInstance::Component)
            }
        }
    }

    /// Instantiates and returns a [`IncomingHttpInstance`] if exported by the [`Instance`].
    ///
    /// # Errors
    ///
    /// Fails if no incoming HTTP bindings are exported by the [`Instance`]
    pub async fn into_incoming_http(self) -> anyhow::Result<IncomingHttpInstance> {
        match self {
            Self::Module(module) => Ok(IncomingHttpInstance::Module(ModuleGuestInstance::from(
                module,
            ))),
            Self::Component(component) => component
                .into_incoming_http()
                .await
                .map(IncomingHttpInstance::Component),
        }
    }

    /// Instantiates and returns a [`LoggingInstance`] if exported by the [`Instance`].
    ///
    /// # Errors
    ///
    /// Fails if no logging bindings are exported by the [`Instance`]
    pub async fn into_logging(self) -> anyhow::Result<LoggingInstance> {
        match self {
            Self::Module(module) => Ok(LoggingInstance::Module(ModuleGuestInstance::from(module))),
            Self::Component(component) => component
                .into_logging()
                .await
                .map(LoggingInstance::Component),
        }
    }
}

#[derive(Debug)]
struct PooledInstances {
    instances: Vec<Instance>,
    limit: Option<NonZeroUsize>,
}

impl Deref for PooledInstances {
    type Target = Vec<Instance>;

    fn deref(&self) -> &Self::Target {
        &self.instances
    }
}

impl PooledInstances {
    fn new(limit: Option<NonZeroUsize>) -> Self {
        let instances = if let Some(limit) = limit {
            Vec::with_capacity(limit.into())
        } else {
            Vec::default()
        };
        Self { instances, limit }
    }

    fn set_limit(&mut self, limit: Option<NonZeroUsize>) -> Option<NonZeroUsize> {
        if let Some(limit) = limit {
            self.instances.truncate(limit.into());
        }
        match limit {
            None => self.limit.take(),
            Some(limit) => self.limit.replace(limit),
        }
    }

    fn push(&mut self, instance: Instance) -> Option<Instance> {
        if self.instances.len() < self.limit.map_or(usize::MAX, Into::into) {
            self.instances.push(instance);
            None
        } else {
            Some(instance)
        }
    }

    fn pop(&mut self) -> Option<Instance> {
        self.instances.pop()
    }
}

/// Actor instance pool, which lazily instantiates actors on demand.
#[derive(Clone, Debug)]
pub struct InstancePool {
    actor: Arc<Actor>,
    instances: Arc<RwLock<PooledInstances>>,
}

impl Deref for InstancePool {
    type Target = Actor;

    fn deref(&self) -> &Self::Target {
        &self.actor
    }
}

/// `PooledInstance`, which will be returned to the [`InstancePool`] when dropped
#[derive(Debug)]
pub struct PooledInstance {
    instance: Option<Instance>,
    instances: Arc<RwLock<PooledInstances>>,
    runtime: Runtime,
}

impl Drop for PooledInstance {
    fn drop(&mut self) {
        if let Some(mut instance) = self.instance.take() {
            task::block_in_place(move || {
                Handle::current().block_on(async {
                    instance.reset(&self.runtime).await;
                    self.instances.write().await.push(instance)
                })
            });
        }
    }
}

/// Converts the [`PooledInstance`] into the inner [`Instance`].
/// Note, that this instance will not be inserted back into the [`InstancePool`] it originated from.
impl From<PooledInstance> for Instance {
    fn from(
        PooledInstance {
            ref mut instance, ..
        }: PooledInstance,
    ) -> Self {
        instance.take().expect("instance missing")
    }
}

impl Deref for PooledInstance {
    type Target = Instance;

    fn deref(&self) -> &Self::Target {
        self.instance.as_ref().expect("instance missing")
    }
}

impl DerefMut for PooledInstance {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.instance.as_mut().expect("instance missing")
    }
}

impl From<Actor> for InstancePool {
    fn from(actor: Actor) -> Self {
        Self::new(actor, None)
    }
}

// TODO: Support component model interfaces
impl InstancePool {
    /// Construct a new [`InstancePool`]
    #[must_use]
    pub fn new(actor: Actor, limit: Option<NonZeroUsize>) -> Self {
        Self {
            actor: Arc::new(actor),
            instances: Arc::new(RwLock::new(PooledInstances::new(limit))),
        }
    }

    /// Return the maximum size of the pool.
    pub async fn get_limit(&self) -> Option<NonZeroUsize> {
        self.instances.read().await.limit
    }

    /// Resize the pool to hold at most `limit` actor instances returning the old value.
    pub async fn set_limit(&self, limit: Option<NonZeroUsize>) -> Option<NonZeroUsize> {
        self.instances.write().await.set_limit(limit)
    }

    /// Resize the pool to be able to hold `limit` actor instances or more, returning the old value.
    pub async fn increase_limit(&self, limit: NonZeroUsize) -> Option<NonZeroUsize> {
        let mut instances = self.instances.write().await;
        if let Some(current) = instances.limit {
            if current < limit {
                instances.set_limit(Some(limit))
            } else {
                Some(current)
            }
        } else {
            instances.set_limit(Some(limit))
        }
    }

    /// Resize the pool to be able to hold `limit` actor instances or less, returning the old value.
    pub async fn decrease_limit(&self, limit: NonZeroUsize) -> Option<NonZeroUsize> {
        let mut instances = self.instances.write().await;
        if let Some(current) = instances.limit {
            if current > limit {
                instances.set_limit(Some(limit))
            } else {
                Some(current)
            }
        } else {
            instances.set_limit(Some(limit))
        }
    }

    /// Instantiate the actor and return an [PooledInstance] on success.
    #[instrument]
    pub async fn instantiate(&self, runtime: Runtime) -> anyhow::Result<PooledInstance> {
        let instance = if let Some(instance) = self.instances.write().await.pop() {
            instance
        } else {
            self.actor
                .instantiate()
                .await
                .context("failed to instantiate actor")?
        };
        Ok(PooledInstance {
            instance: Some(instance),
            instances: Arc::clone(&self.instances),
            runtime,
        })
    }
}
