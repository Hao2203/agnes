use std::marker::PhantomData;

use crate::access::AccessSet;
use crate::system_param::{ParamError, SystemParam};
use crate::world::World;

/// A failure while running a system.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SystemError {
    /// A parameter could not be prepared.
    #[error("system `{system}`: {source}")]
    Param {
        /// The system's type name.
        system: &'static str,
        /// The parameter failure.
        #[source]
        source: ParamError,
    },
}

/// The runtime contract for a system.
///
/// Most users implement systems as ordinary functions and let
/// [`IntoSystem`] build a [`FunctionSystem`].
pub trait System: Send + 'static {
    /// The diagnostic name of the system.
    fn name(&self) -> &'static str;

    /// The values the system declares access to.
    fn access(&self) -> &AccessSet;

    /// Creates per-system state on first use.
    fn initialize(&mut self, world: &mut World);

    /// Runs the system and applies deferred work.
    ///
    /// # Errors
    ///
    /// Returns [`SystemError`] when a required parameter is unavailable.
    fn run(&mut self, world: &mut World) -> Result<(), SystemError>;
}

/// A function whose parameters can be extracted from the world.
///
/// This trait is implemented for functions with 0–8 [`SystemParam`]
/// arguments. It is the bridge that turns ordinary functions into systems
/// without runtime reflection.
pub trait SystemParamFunction<Marker>: Send + Sync + 'static {
    /// The tuple of system parameters.
    type In: SystemParam;

    /// Invokes the function with extracted parameter values.
    fn run<'a>(&'a mut self, input: <Self::In as SystemParam>::Item<'a, 'a>);
}

/// Converts a function into a [`System`].
pub trait IntoSystem<Marker> {
    /// The resulting system type.
    type System: System;

    /// Creates the system.
    fn into_system(self) -> Self::System;
}

impl<Marker, F> IntoSystem<Marker> for F
where
    Marker: 'static,
    F: SystemParamFunction<Marker>,
{
    type System = FunctionSystem<Marker, F>;

    fn into_system(self) -> Self::System {
        FunctionSystem::new(self)
    }
}

/// The [`System`] implementation for ordinary functions.
pub struct FunctionSystem<Marker, F>
where
    Marker: 'static,
    F: SystemParamFunction<Marker>,
{
    function: F,
    state: Option<<F::In as SystemParam>::State>,
    access: AccessSet,
    marker: PhantomData<fn() -> Marker>,
}

impl<Marker, F> FunctionSystem<Marker, F>
where
    Marker: 'static,
    F: SystemParamFunction<Marker>,
{
    pub(crate) fn new(function: F) -> Self {
        Self {
            function,
            state: None,
            access: <F::In as SystemParam>::access(),
            marker: PhantomData,
        }
    }
}

impl<Marker, F> System for FunctionSystem<Marker, F>
where
    Marker: 'static,
    F: SystemParamFunction<Marker>,
{
    fn name(&self) -> &'static str {
        std::any::type_name::<F>()
    }

    fn access(&self) -> &AccessSet {
        &self.access
    }

    fn initialize(&mut self, world: &mut World) {
        if self.state.is_none() {
            self.state = Some(<F::In as SystemParam>::init_state(world));
        }
    }

    fn run(&mut self, world: &mut World) -> Result<(), SystemError> {
        <F::In as SystemParam>::validate(world).map_err(|source| SystemError::Param {
            system: self.name(),
            source,
        })?;

        self.initialize(world);

        {
            let Self {
                function, state, ..
            } = self;
            let state = state.as_mut().expect("system state initialized above");
            let input = <F::In as SystemParam>::get_param(world, state);
            function.run(input);
        }

        let Self { state, .. } = self;
        let state = state.as_mut().expect("system state initialized above");
        <F::In as SystemParam>::apply(world, state);
        Ok(())
    }
}

impl<Func> SystemParamFunction<fn()> for Func
where
    Func: Send + Sync + 'static,
    for<'a> &'a mut Func: FnMut(),
{
    type In = ();

    fn run(&mut self, input: ()) {
        fn call_inner(mut function: impl FnMut()) {
            function();
        }

        let () = input;
        call_inner(self);
    }
}

macro_rules! impl_system_param_function {
    ($($param:ident),+) => {
        impl<Func, $($param),*> SystemParamFunction<fn($($param),*)> for Func
        where
            Func: Send + Sync + 'static,
            $($param: SystemParam + 'static,)*
            for<'a, 'b> &'a mut Func:
                FnMut($($param),*) +
                FnMut($(<$param as SystemParam>::Item<'a, 'b>),*),
        {
            type In = ($($param,)*);

            #[allow(non_snake_case, clippy::too_many_arguments)]
            fn run<'a>(&'a mut self, input: <Self::In as SystemParam>::Item<'a, 'a>) {
                fn call_inner<$($param),*>(
                    mut function: impl FnMut($($param),*),
                    $($param: $param),*
                ) {
                    function($($param),*);
                }

                let ($($param,)*) = input;
                call_inner(self, $($param),*);
            }
        }
    };
}

impl_system_param_function!(P0);
impl_system_param_function!(P0, P1);
impl_system_param_function!(P0, P1, P2);
impl_system_param_function!(P0, P1, P2, P3);
impl_system_param_function!(P0, P1, P2, P3, P4);
impl_system_param_function!(P0, P1, P2, P3, P4, P5);
impl_system_param_function!(P0, P1, P2, P3, P4, P5, P6);
impl_system_param_function!(P0, P1, P2, P3, P4, P5, P6, P7);
