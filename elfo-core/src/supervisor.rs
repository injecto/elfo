use std::{any::Any, fmt::Display, future::Future, hash::Hash, panic::AssertUnwindSafe, sync::Arc};

use dashmap::DashMap;
use futures::FutureExt;
use fxhash::FxBuildHasher;
use parking_lot::RwLock;
use serde::Deserialize;
use tracing::{error, error_span, Instrument, Span};

use elfo_macros::msg_internal as msg;

use crate::{
    addr::Addr,
    context::Context,
    envelope::Envelope,
    errors::TrySendError,
    exec::{BoxedError, Exec, ExecResult},
    messages,
    object::{Object, ObjectArc},
    routers::{Outcome, Router},
};

pub(crate) struct Supervisor<R: Router, C, X> {
    name: String,
    context: Context,
    objects: DashMap<R::Key, ObjectArc, FxBuildHasher>,
    router: R,
    exec: X,
    config: RwLock<Option<Arc<C>>>,
}

impl<R, C, X> Supervisor<R, C, X>
where
    R: Router,
    R::Key: Clone + Hash + Eq + Display,
    X: Exec<Context<C, R::Key>>,
    <X::Output as Future>::Output: ExecResult,
    C: for<'de> Deserialize<'de> + Send + Sync + 'static,
{
    pub(crate) fn new(ctx: Context, name: String, exec: X, router: R) -> Self {
        Self {
            name,
            context: ctx,
            objects: DashMap::default(),
            router,
            exec,
            config: RwLock::new(None),
        }
    }

    pub(crate) fn handle(&self, envelope: Envelope) -> RouteReport {
        msg!(match &envelope {
            messages::ValidateConfig { config } => match config.decode::<C>() {
                Ok(config) => {
                    let mut envelope = envelope;
                    envelope.set_message(messages::ValidateConfig { config });
                    self.do_handle(envelope, Outcome::Broadcast)
                }
                Err(reason) => {
                    msg!(match envelope {
                        (messages::ValidateConfig { .. }, token) => {
                            let reject = messages::ConfigRejected { reason };
                            self.context.respond(token, Err(reject));
                        }
                        _ => unreachable!(),
                    });
                    RouteReport::Done
                }
            },
            messages::UpdateConfig { config } => match config.decode::<C>() {
                Ok(config) => {
                    *self.config.write() = config.get().cloned();

                    let mut envelope = envelope;
                    envelope.set_message(messages::UpdateConfig { config });
                    self.do_handle(envelope, Outcome::Broadcast)
                }
                Err(reason) => {
                    msg!(match envelope {
                        (messages::UpdateConfig { .. }, token) => {
                            let reject = messages::ConfigRejected { reason };
                            self.context.respond(token, Err(reject));
                        }
                        _ => unreachable!(),
                    });
                    RouteReport::Done
                }
            },
            _ => {
                let outcome = self.router.route(&envelope);
                self.do_handle(envelope, outcome)
            }
        })
    }

    pub(crate) fn do_handle(&self, envelope: Envelope, outcome: Outcome<R::Key>) -> RouteReport {
        match outcome {
            Outcome::Unicast(key) => {
                let object = ward!(self.objects.get(&key), {
                    self.objects
                        .entry(key.clone())
                        .or_insert_with(|| self.spawn(key))
                        .downgrade()
                });

                let actor = object.as_actor().expect("supervisor stores only actors");
                match actor.mailbox.try_send(envelope) {
                    Ok(()) => RouteReport::Done,
                    Err(TrySendError::Full(envelope)) => RouteReport::Wait(object.addr(), envelope),
                    Err(TrySendError::Closed(envelope)) => RouteReport::Closed(envelope),
                }
            }
            Outcome::Broadcast => {
                let mut waiters = Vec::new();
                let mut someone = false;

                // TODO: avoid the loop in `try_send` case.
                for object in self.objects.iter() {
                    // TODO: we shouldn't clone `envelope` for the last object in a sequence.
                    let envelope = ward!(
                        envelope.duplicate(self.context.book()),
                        return RouteReport::Done // A requester has died.
                    );

                    let actor = object.as_actor().expect("supervisor stores only actors");

                    match actor.mailbox.try_send(envelope) {
                        Ok(_) => someone = true,
                        Err(TrySendError::Full(envelope)) => {
                            waiters.push((object.addr(), envelope))
                        }
                        Err(TrySendError::Closed(_)) => {}
                    }
                }

                if waiters.is_empty() {
                    if someone {
                        RouteReport::Done
                    } else {
                        RouteReport::Closed(envelope)
                    }
                } else {
                    RouteReport::WaitAll(someone, waiters)
                }
            }
            Outcome::Discard => RouteReport::Done,
        }
    }

    fn on_actor_finished(&self, addr: Addr, result: ActorResult) {
        match result {
            ActorResult::Completed => {}
            ActorResult::Failed(error) => {
                error!(%error, "actor failed");
            }
            ActorResult::Panicked(panic) => {
                let error = payload_to_string(&*panic);
                error!(%error, "actor panicked");
            }
        }
    }

    fn spawn(&self, key: R::Key) -> ObjectArc {
        let entry = self.context.book().vacant_entry();
        let addr = entry.addr();

        let full_name = format!("{}.{}", self.name, key);
        let span = error_span!(parent: Span::none(), "", actor = %full_name);
        let _entered = span.enter();

        let config = self
            .config
            .read()
            .as_ref()
            .cloned()
            .expect("config is unset");

        let ctx = self
            .context
            .clone()
            .with_addr(addr)
            .with_key(key)
            .with_config(config);

        // TODO: protect against panics (for `fn(..) -> impl Future`).
        let fut = self.exec.exec(ctx);
        let fut = async move {
            let fut = AssertUnwindSafe(async { fut.await.unify() }).catch_unwind();
            let _res = match fut.await {
                Ok(Ok(())) => ActorResult::Completed,
                Ok(Err(err)) => ActorResult::Failed(err),
                Err(panic) => ActorResult::Panicked(panic),
            };

            // TODO
        };

        drop(_entered);
        let handle = tokio::spawn(fut.instrument(span));

        entry.insert(Object::new_actor(addr, handle));

        self.context.book().get_owned(addr).expect("just created")
    }
}

enum ActorResult {
    Completed,
    Failed(BoxedError),
    Panicked(Box<dyn Any + Send + 'static>),
}

fn payload_to_string(payload: &dyn Any) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "(unsupported payload)".to_string()
    }
}

pub(crate) enum RouteReport {
    Done,
    Closed(Envelope),
    Wait(Addr, Envelope),
    WaitAll(bool, Vec<(Addr, Envelope)>),
}
