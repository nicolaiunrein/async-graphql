use actix::{
    Actor, ActorContext, ActorFuture, AsyncContext, ContextFutureSpawner, StreamHandler, WrapFuture,
};
use actix_web_actors::ws::{Message, ProtocolError, WebsocketContext};
use async_graphql::{ObjectType, Schema, SubscriptionType, WebSocketTransport};
use bytes::Bytes;
use futures::channel::mpsc;
use futures::SinkExt;
use std::time::{Duration, Instant};

pub struct WsSession<Query, Mutation, Subscription> {
    schema: Schema<Query, Mutation, Subscription>,
    hb: Instant,
    sink: Option<mpsc::Sender<Bytes>>,
}

impl<Query, Mutation, Subscription> WsSession<Query, Mutation, Subscription>
where
    Query: ObjectType + Send + Sync + 'static,
    Mutation: ObjectType + Send + Sync + 'static,
    Subscription: SubscriptionType + Send + Sync + 'static,
{
    pub fn new(schema: Schema<Query, Mutation, Subscription>) -> Self {
        Self {
            schema,
            hb: Instant::now(),
            sink: None,
        }
    }

    fn hb(&self, ctx: &mut WebsocketContext<Self>) {
        ctx.run_interval(Duration::new(1, 0), |act, ctx| {
            if Instant::now().duration_since(act.hb) > Duration::new(10, 0) {
                ctx.stop();
            }
        });
    }
}

impl<Query, Mutation, Subscription> Actor for WsSession<Query, Mutation, Subscription>
where
    Query: ObjectType + Sync + Send + 'static,
    Mutation: ObjectType + Sync + Send + 'static,
    Subscription: SubscriptionType + Send + Sync + 'static,
{
    type Context = WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.hb(ctx);
        let schema = self.schema.clone();
        async move {
            schema
                .clone()
                .subscription_connection(WebSocketTransport::default())
                .await
        }
        .into_actor(self)
        .then(|(sink, stream), actor, ctx| {
            actor.sink = Some(sink);
            ctx.add_stream(stream);
            async {}.into_actor(actor)
        })
        .wait(ctx);
    }
}

impl<Query, Mutation, Subscription> StreamHandler<Result<Message, ProtocolError>>
    for WsSession<Query, Mutation, Subscription>
where
    Query: ObjectType + Sync + Send + 'static,
    Mutation: ObjectType + Sync + Send + 'static,
    Subscription: SubscriptionType + Send + Sync + 'static,
{
    fn handle(&mut self, msg: Result<Message, ProtocolError>, ctx: &mut Self::Context) {
        let msg = match msg {
            Err(_) => {
                ctx.stop();
                return;
            }
            Ok(msg) => msg,
        };

        match msg {
            Message::Ping(msg) => {
                self.hb = Instant::now();
                ctx.pong(&msg);
            }
            Message::Pong(_) => {
                self.hb = Instant::now();
            }
            Message::Text(s) => {
                if let Some(mut sink) = self.sink.clone() {
                    async move { sink.send(s.into()).await }
                        .into_actor(self)
                        .then(|_, actor, _| async {}.into_actor(actor))
                        .wait(ctx);
                }
            }
            Message::Binary(_) | Message::Close(_) | Message::Continuation(_) => {
                ctx.stop();
            }
            Message::Nop => {}
        }
    }
}

impl<Query, Mutation, Subscription> StreamHandler<Bytes>
    for WsSession<Query, Mutation, Subscription>
where
    Query: ObjectType + Send + Sync + 'static,
    Mutation: ObjectType + Send + Sync + 'static,
    Subscription: SubscriptionType + Send + Sync + 'static,
{
    fn handle(&mut self, data: Bytes, ctx: &mut Self::Context) {
        ctx.text(unsafe { std::str::from_utf8_unchecked(&data) });
    }
}
