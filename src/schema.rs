use crate::context::Data;
use crate::extensions::{BoxExtension, Extension};
use crate::model::__DirectiveLocation;
use crate::query::QueryBuilder;
use crate::registry::{Directive, InputValue, Registry};
use crate::subscription::{create_connection, SubscriptionStub, SubscriptionTransport};
use crate::types::QueryRoot;
use crate::validation::check_rules;
use crate::{
    ContextSelectionSet, ObjectType, QueryError, QueryParseError, Result, SubscriptionStream,
    SubscriptionType, Type, Variables,
};
use bytes::Bytes;
use futures::channel::mpsc;
use futures::lock::Mutex;
use futures::SinkExt;
use graphql_parser::parse_query;
use graphql_parser::query::{
    Definition, Field, FragmentDefinition, OperationDefinition, Selection,
};
use once_cell::sync::Lazy;
use slab::Slab;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

type MsgSender = mpsc::Sender<Arc<dyn Any + Sync + Send>>;

pub(crate) static SUBSCRIPTION_SENDERS: Lazy<Mutex<Slab<MsgSender>>> = Lazy::new(Default::default);

pub(crate) struct SchemaInner<Query, Mutation, Subscription> {
    pub(crate) query: QueryRoot<Query>,
    pub(crate) mutation: Mutation,
    pub(crate) subscription: Subscription,
    pub(crate) registry: Registry,
    pub(crate) data: Data,
    pub(crate) complexity: Option<usize>,
    pub(crate) depth: Option<usize>,
    pub(crate) extensions: Vec<Box<dyn Fn() -> BoxExtension + Send + Sync>>,
}

/// Schema builder
pub struct SchemaBuilder<Query, Mutation, Subscription>(SchemaInner<Query, Mutation, Subscription>);

impl<Query: ObjectType, Mutation: ObjectType, Subscription: SubscriptionType>
    SchemaBuilder<Query, Mutation, Subscription>
{
    /// Disable introspection query
    pub fn disable_introspection(mut self) -> Self {
        self.0.query.disable_introspection = true;
        self
    }

    /// Set limit complexity, Default no limit.
    pub fn limit_complexity(mut self, complexity: usize) -> Self {
        self.0.complexity = Some(complexity);
        self
    }

    /// Set limit complexity, Default no limit.
    pub fn limit_depth(mut self, depth: usize) -> Self {
        self.0.depth = Some(depth);
        self
    }

    /// Add an extension
    pub fn extension<F: Fn() -> E + Send + Sync + 'static, E: Extension>(
        mut self,
        extension_factory: F,
    ) -> Self {
        self.0
            .extensions
            .push(Box::new(move || Box::new(extension_factory())));
        self
    }

    /// Add a global data that can be accessed in the `Context`.
    pub fn data<D: Any + Send + Sync>(mut self, data: D) -> Self {
        self.0.data.insert(data);
        self
    }

    /// Build schema.
    pub fn finish(self) -> Schema<Query, Mutation, Subscription> {
        Schema(Arc::new(self.0))
    }
}

/// GraphQL schema
pub struct Schema<Query, Mutation, Subscription>(
    pub(crate) Arc<SchemaInner<Query, Mutation, Subscription>>,
);

impl<Query, Mutation, Subscription> Clone for Schema<Query, Mutation, Subscription> {
    fn clone(&self) -> Self {
        Schema(self.0.clone())
    }
}

impl<Query, Mutation, Subscription> Schema<Query, Mutation, Subscription>
where
    Query: ObjectType + Send + Sync + 'static,
    Mutation: ObjectType + Send + Sync + 'static,
    Subscription: SubscriptionType + Send + Sync + 'static,
{
    /// Create a schema builder
    ///
    /// The root object for the query and Mutation needs to be specified.
    /// If there is no mutation, you can use `EmptyMutation`.
    /// If there is no subscription, you can use `EmptySubscription`.
    pub fn build(
        query: Query,
        mutation: Mutation,
        subscription: Subscription,
    ) -> SchemaBuilder<Query, Mutation, Subscription> {
        let mut registry = Registry {
            types: Default::default(),
            directives: Default::default(),
            implements: Default::default(),
            query_type: Query::type_name().to_string(),
            mutation_type: if Mutation::is_empty() {
                None
            } else {
                Some(Mutation::type_name().to_string())
            },
            subscription_type: if Subscription::is_empty() {
                None
            } else {
                Some(Subscription::type_name().to_string())
            },
        };

        registry.add_directive(Directive {
            name: "include",
            description: Some("Directs the executor to include this field or fragment only when the `if` argument is true."),
            locations: vec![
                __DirectiveLocation::FIELD,
                __DirectiveLocation::FRAGMENT_SPREAD,
                __DirectiveLocation::INLINE_FRAGMENT
            ],
            args: {
                let mut args = HashMap::new();
                args.insert("if", InputValue {
                    name: "if",
                    description: Some("Included when true."),
                    ty: "Boolean!".to_string(),
                    default_value: None,
                    validator: None,
                });
                args
            }
        });

        registry.add_directive(Directive {
            name: "skip",
            description: Some("Directs the executor to skip this field or fragment when the `if` argument is true."),
            locations: vec![
                __DirectiveLocation::FIELD,
                __DirectiveLocation::FRAGMENT_SPREAD,
                __DirectiveLocation::INLINE_FRAGMENT
            ],
            args: {
                let mut args = HashMap::new();
                args.insert("if", InputValue {
                    name: "if",
                    description: Some("Skipped when true."),
                    ty: "Boolean!".to_string(),
                    default_value: None,
                    validator: None,
                });
                args
            }
        });

        // register scalars
        bool::create_type_info(&mut registry);
        i32::create_type_info(&mut registry);
        f32::create_type_info(&mut registry);
        String::create_type_info(&mut registry);

        QueryRoot::<Query>::create_type_info(&mut registry);
        if !Mutation::is_empty() {
            Mutation::create_type_info(&mut registry);
        }
        if !Subscription::is_empty() {
            Subscription::create_type_info(&mut registry);
        }

        SchemaBuilder(SchemaInner {
            query: QueryRoot {
                inner: query,
                disable_introspection: false,
            },
            mutation,
            subscription,
            registry,
            data: Default::default(),
            complexity: None,
            depth: None,
            extensions: Default::default(),
        })
    }

    /// Create a schema
    pub fn new(
        query: Query,
        mutation: Mutation,
        subscription: Subscription,
    ) -> Schema<Query, Mutation, Subscription> {
        Self::build(query, mutation, subscription).finish()
    }

    /// Start a query and return `QueryBuilder`.
    pub fn query<'a>(&'a self, source: &'a str) -> QueryBuilder<'a, Query, Mutation, Subscription> {
        QueryBuilder {
            extensions: self.0.extensions.iter().map(|factory| factory()).collect(),
            schema: self,
            source,
            operation_name: None,
            variables: None,
            data: &self.0.data,
        }
    }

    /// Create subscription stub, typically called inside the `SubscriptionTransport::handle_request` method/
    pub fn create_subscription_stub(
        &self,
        source: &str,
        operation_name: Option<&str>,
        variables: Variables,
    ) -> Result<SubscriptionStub<Query, Mutation, Subscription>>
    where
        Self: Sized,
    {
        let document = parse_query(source).map_err(|err| QueryParseError(err.to_string()))?;
        check_rules(&self.0.registry, &document)?;

        let mut fragments = HashMap::new();
        let mut subscription = None;

        for definition in document.definitions {
            match definition {
                Definition::Operation(OperationDefinition::Subscription(s)) => {
                    if s.name.as_deref() == operation_name {
                        subscription = Some(s);
                        break;
                    }
                }
                Definition::Fragment(fragment) => {
                    fragments.insert(fragment.name.clone(), fragment);
                }
                _ => {}
            }
        }

        let subscription = subscription.ok_or(if let Some(name) = operation_name {
            QueryError::UnknownOperationNamed {
                name: name.to_string(),
            }
        } else {
            QueryError::MissingOperation
        })?;

        let mut types = HashMap::new();
        let resolve_id = AtomicUsize::default();
        let ctx = ContextSelectionSet {
            path_node: None,
            extensions: &[],
            item: &subscription.selection_set,
            resolve_id: &resolve_id,
            variables: &variables,
            variable_definitions: Some(&subscription.variable_definitions),
            registry: &self.0.registry,
            data: &Default::default(),
            fragments: &fragments,
        };
        create_subscription_types::<Subscription>(&ctx, &fragments, &mut types)?;
        Ok(SubscriptionStub {
            schema: self.clone(),
            types,
            variables,
            variable_definitions: subscription.variable_definitions,
            fragments,
        })
    }

    /// Create subscription connection, returns `Sink` and `Stream`.
    pub async fn subscription_connection<T: SubscriptionTransport>(
        &self,
        transport: T,
    ) -> (
        mpsc::Sender<Bytes>,
        SubscriptionStream<Query, Mutation, Subscription, T>,
    ) {
        create_connection(self, transport).await
    }
}

fn create_subscription_types<T: SubscriptionType>(
    ctx: &ContextSelectionSet<'_>,
    fragments: &HashMap<String, FragmentDefinition>,
    types: &mut HashMap<TypeId, Field>,
) -> Result<()> {
    for selection in &ctx.items {
        match selection {
            Selection::Field(field) => {
                if ctx.is_skip(&field.directives)? {
                    continue;
                }
                T::create_type(field, types)?;
            }
            Selection::FragmentSpread(fragment_spread) => {
                if ctx.is_skip(&fragment_spread.directives)? {
                    continue;
                }

                if let Some(fragment) = fragments.get(&fragment_spread.fragment_name) {
                    create_subscription_types::<T>(
                        &ctx.with_selection_set(&fragment.selection_set),
                        fragments,
                        types,
                    )?;
                } else {
                    return Err(QueryError::UnknownFragment {
                        name: fragment_spread.fragment_name.clone(),
                    }
                    .into());
                }
            }
            Selection::InlineFragment(inline_fragment) => {
                if ctx.is_skip(&inline_fragment.directives)? {
                    continue;
                }
                create_subscription_types::<T>(
                    &ctx.with_selection_set(&inline_fragment.selection_set),
                    fragments,
                    types,
                )?;
            }
        }
    }
    Ok(())
}

/// Publish a message that will be pushed to all subscribed clients.
pub async fn publish<T: Any + Send + Sync + Sized>(msg: T) {
    let mut senders = SUBSCRIPTION_SENDERS.lock().await;
    let msg = Arc::new(msg);
    let mut remove = Vec::new();
    for (id, sender) in senders.iter_mut() {
        if sender.send(msg.clone()).await.is_err() {
            remove.push(id);
        }
    }
    for id in remove {
        senders.remove(id);
    }
}
