use crate::access::{Access, AccessMode};
use crate::commands::{CommandQueue, Commands};
use crate::message::{Message, MessageReader, MessageWriter, ReaderId};
use crate::query::{Query, QueryData, QueryFilter};
use crate::resource::{Res, ResMut, Resource};
use crate::world::World;

/// A problem encountered while preparing a system parameter.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ParamError {
    /// A required resource was not present when the system ran.
    #[error("required resource `{resource}` is missing")]
    MissingResource {
        /// The fully qualified resource type name.
        resource: &'static str,
    },
}

/// Declares how a system parameter is initialized and extracted from the world.
///
/// The associated [`State`](SystemParam::State) is created once per system and
/// reused on every run. Query parameters have no state, while message readers
/// keep their cursor identity and commands keep a queued command buffer.
pub trait SystemParam {
    /// Per-system state created on first use.
    type State: Send + 'static;

    /// The value handed to the system function.
    type Item<'w, 's>;

    /// Creates the per-system state.
    fn init_state(world: &mut World) -> Self::State;

    /// Checks runtime requirements that cannot be known statically.
    ///
    /// # Errors
    ///
    /// Returns [`ParamError`] when a required resource or message queue is
    /// missing.
    fn validate(_world: &World) -> Result<(), ParamError> {
        Ok(())
    }

    /// Extracts the parameter value from the world and its state.
    fn get_param<'w, 's>(world: &'w World, state: &'s mut Self::State) -> Self::Item<'w, 's>;

    /// Applies deferred work queued through this parameter.
    ///
    /// Only `Commands` currently uses this hook; all other parameters do
    /// nothing.
    fn apply(_world: &mut World, _state: &mut Self::State) {}

    /// The values this parameter reads or writes.
    ///
    /// Collected once when the system is registered; returning an iterator
    /// keeps the per-parameter declarations allocation-free.
    fn access() -> impl Iterator<Item = Access> {
        std::iter::empty()
    }
}

impl SystemParam for () {
    type State = ();
    type Item<'w, 's> = ();

    fn init_state(_world: &mut World) -> Self::State {}

    fn get_param<'w, 's>(_world: &'w World, _state: &'s mut Self::State) -> Self::Item<'w, 's> {}
}

impl<'w, T: Resource> SystemParam for Res<'w, T> {
    type State = ();
    type Item<'a, 's> = Res<'a, T>;

    fn init_state(_world: &mut World) -> Self::State {}

    fn validate(world: &World) -> Result<(), ParamError> {
        if world.contains_resource::<T>() {
            Ok(())
        } else {
            Err(ParamError::MissingResource {
                resource: std::any::type_name::<T>(),
            })
        }
    }

    fn get_param<'a, 's>(world: &'a World, _state: &'s mut Self::State) -> Self::Item<'a, 's> {
        Res::new(
            world
                .resource::<T>()
                .expect("validate checked that the resource exists"),
        )
    }

    fn access() -> impl Iterator<Item = Access> {
        std::iter::once(Access::resource::<T>(AccessMode::Read))
    }
}

impl<'w, T: Resource> SystemParam for ResMut<'w, T> {
    type State = ();
    type Item<'a, 's> = ResMut<'a, T>;

    fn init_state(_world: &mut World) -> Self::State {}

    fn validate(world: &World) -> Result<(), ParamError> {
        if world.contains_resource::<T>() {
            Ok(())
        } else {
            Err(ParamError::MissingResource {
                resource: std::any::type_name::<T>(),
            })
        }
    }

    fn get_param<'a, 's>(world: &'a World, _state: &'s mut Self::State) -> Self::Item<'a, 's> {
        ResMut::new(
            world
                .resource_mut::<T>()
                .expect("validate checked that the resource exists"),
        )
    }

    fn access() -> impl Iterator<Item = Access> {
        std::iter::once(Access::resource::<T>(AccessMode::Write))
    }
}

impl<'w, T: Resource> SystemParam for Option<Res<'w, T>> {
    type State = ();
    type Item<'a, 's> = Option<Res<'a, T>>;

    fn init_state(_world: &mut World) -> Self::State {}

    fn get_param<'a, 's>(world: &'a World, _state: &'s mut Self::State) -> Self::Item<'a, 's> {
        world.resource::<T>().map(Res::new)
    }

    fn access() -> impl Iterator<Item = Access> {
        std::iter::once(Access::resource::<T>(AccessMode::Read))
    }
}

impl<'w, D, F> SystemParam for Query<'w, D, F>
where
    D: QueryData + 'static,
    F: QueryFilter + 'static,
{
    type State = ();
    type Item<'a, 's> = Query<'a, D, F>;

    fn init_state(_world: &mut World) -> Self::State {}

    fn get_param<'a, 's>(world: &'a World, _state: &'s mut Self::State) -> Self::Item<'a, 's> {
        Query::new(world)
    }

    fn access() -> impl Iterator<Item = Access> {
        D::access()
    }
}

impl<'w, T: Message> SystemParam for MessageWriter<'w, T> {
    type State = ();
    type Item<'a, 's> = MessageWriter<'a, T>;

    fn init_state(world: &mut World) -> Self::State {
        world.register_message::<T>();
    }

    fn get_param<'a, 's>(world: &'a World, _state: &'s mut Self::State) -> Self::Item<'a, 's> {
        MessageWriter::new(
            world
                .message_queue::<T>()
                .expect("message queue initialized by init_state"),
        )
    }

    fn access() -> impl Iterator<Item = Access> {
        std::iter::once(Access::message::<T>(AccessMode::Write))
    }
}

impl<'w, T: Message> SystemParam for MessageReader<'w, T> {
    type State = ReaderId;
    type Item<'a, 's> = MessageReader<'a, T>;

    fn init_state(world: &mut World) -> Self::State {
        world.register_message_reader::<T>()
    }

    fn get_param<'a, 's>(world: &'a World, state: &'s mut Self::State) -> Self::Item<'a, 's> {
        MessageReader::new(
            world
                .message_queue::<T>()
                .expect("message queue initialized by init_state"),
            *state,
        )
    }

    fn access() -> impl Iterator<Item = Access> {
        std::iter::once(Access::message::<T>(AccessMode::Read))
    }
}

impl<'w, 's> SystemParam for Commands<'w, 's> {
    type State = CommandQueue;
    type Item<'a, 'b> = Commands<'a, 'b>;

    fn init_state(_world: &mut World) -> Self::State {
        CommandQueue::new()
    }

    fn get_param<'a, 'b>(world: &'a World, state: &'b mut Self::State) -> Self::Item<'a, 'b> {
        Commands::new(world, state)
    }

    fn apply(world: &mut World, state: &mut Self::State) {
        state.apply(world);
    }
}

macro_rules! impl_system_param_tuple {
    ($($param:ident),+) => {
        impl<$($param: SystemParam),+> SystemParam for ($($param,)+) {
            type State = ($($param::State,)+);
            type Item<'w, 's> = ($($param::Item<'w, 's>,)+);

            fn init_state(world: &mut World) -> Self::State {
                ($($param::init_state(world),)+)
            }

            fn validate(world: &World) -> Result<(), ParamError> {
                $($param::validate(world)?;)+
                Ok(())
            }

            #[allow(non_snake_case)]
            fn get_param<'w, 's>(
                world: &'w World,
                state: &'s mut Self::State,
            ) -> Self::Item<'w, 's> {
                let ($($param,)+) = state;
                ($($param::get_param(world, $param),)+)
            }

            #[allow(non_snake_case)]
            fn apply(world: &mut World, state: &mut Self::State) {
                let ($($param,)+) = state;
                $($param::apply(world, $param);)+
            }

            fn access() -> impl Iterator<Item = Access> {
                std::iter::empty() $(.chain($param::access()))+
            }
        }
    };
}

impl_system_param_tuple!(P0);
impl_system_param_tuple!(P0, P1);
impl_system_param_tuple!(P0, P1, P2);
impl_system_param_tuple!(P0, P1, P2, P3);
impl_system_param_tuple!(P0, P1, P2, P3, P4);
impl_system_param_tuple!(P0, P1, P2, P3, P4, P5);
impl_system_param_tuple!(P0, P1, P2, P3, P4, P5, P6);
impl_system_param_tuple!(P0, P1, P2, P3, P4, P5, P6, P7);
