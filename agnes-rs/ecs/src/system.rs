use std::fmt;
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
    /// The system itself returned an error.
    #[error("system `{system}` failed: {source}")]
    Failed {
        /// The system's type name.
        system: &'static str,
        /// The error the system returned.
        #[source]
        source: anyhow::Error,
    },
}

/// A failure reported by a system function.
///
/// System functions may return any error convertible into [`anyhow::Error`].
/// The runtime wraps it in this crate-owned type so the [`SystemOut`] and
/// [`SystemParamFunction`] contracts do not expose a third-party error type,
/// then attributes it to the system as [`SystemError::Failed`].
///
/// The original error is preserved: recover it with
/// [`downcast_ref`](SystemFailure::downcast_ref) or through the `source` field
/// of [`SystemError::Failed`].
#[derive(Debug)]
pub struct SystemFailure(anyhow::Error);

impl SystemFailure {
    /// Wraps any error convertible into [`anyhow::Error`].
    pub fn new(source: impl Into<anyhow::Error>) -> Self {
        Self(source.into())
    }

    /// The original error, if it has type `E`.
    pub fn downcast_ref<E>(&self) -> Option<&E>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        self.0.downcast_ref::<E>()
    }

    fn into_inner(self) -> anyhow::Error {
        self.0
    }
}

impl From<anyhow::Error> for SystemFailure {
    fn from(source: anyhow::Error) -> Self {
        Self(source)
    }
}

impl fmt::Display for SystemFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

impl std::error::Error for SystemFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.0)
    }
}

/// The output of a system function.
///
/// A system function may return nothing or a `Result<(), E>`; an `Err` is
/// wrapped in a [`SystemFailure`] and reported to the caller of
/// [`System::run`] as [`SystemError::Failed`].
pub trait SystemOut {
    /// Converts the function output into the fallible system result.
    ///
    /// # Errors
    ///
    /// Returns [`SystemFailure`] carrying the error of a `Result` output, or
    /// `Ok` for `()`.
    fn into_system_result(self) -> Result<(), SystemFailure>;
}

impl SystemOut for () {
    fn into_system_result(self) -> Result<(), SystemFailure> {
        Ok(())
    }
}

impl<E> SystemOut for Result<(), E>
where
    E: Into<anyhow::Error>,
{
    fn into_system_result(self) -> Result<(), SystemFailure> {
        self.map_err(SystemFailure::new)
    }
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
    /// Returns [`SystemError`] when a required parameter is unavailable or the
    /// system function returned an error.
    fn run(&mut self, world: &mut World) -> Result<(), SystemError>;
}

/// A function whose parameters can be extracted from the world.
///
/// This trait is implemented for functions with 0–8 [`SystemParam`]
/// arguments. It is the bridge that turns ordinary functions into systems
/// without runtime reflection. Functions may return nothing or a
/// `Result<(), E>`; see [`SystemOut`].
pub trait SystemParamFunction<Marker>: Send + Sync + 'static {
    /// The tuple of system parameters.
    type In: SystemParam;

    /// Invokes the function with extracted parameter values.
    ///
    /// # Errors
    ///
    /// Returns [`SystemFailure`] when the function itself returned `Err`.
    fn run<'a>(
        &'a mut self,
        input: <Self::In as SystemParam>::Item<'a, 'a>,
    ) -> Result<(), SystemFailure>;
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

        let name = self.name();
        let outcome = {
            let Self {
                function, state, ..
            } = self;
            let state = state.as_mut().expect("system state initialized above");
            let input = <F::In as SystemParam>::get_param(world, state);
            function.run(input).map_err(|failure| SystemError::Failed {
                system: name,
                source: failure.into_inner(),
            })
        };

        let Self { state, .. } = self;
        let state = state.as_mut().expect("system state initialized above");
        <F::In as SystemParam>::apply(world, state);
        outcome
    }
}

macro_rules! impl_system_param_function {
    ($($param:ident),*) => {
        impl<Func, Ret, $($param),*> SystemParamFunction<fn($($param),*) -> Ret> for Func
        where
            Func: Send + Sync + 'static,
            Ret: SystemOut,
            $($param: SystemParam + 'static,)*
            for<'a, 'b> &'a mut Func:
                FnMut($($param),*) -> Ret +
                FnMut($(<$param as SystemParam>::Item<'a, 'b>),*) -> Ret,
        {
            type In = ($($param,)*);

            #[allow(non_snake_case, clippy::too_many_arguments)]
            fn run<'a>(
                &'a mut self,
                input: <Self::In as SystemParam>::Item<'a, 'a>,
            ) -> Result<(), SystemFailure> {
                fn call_inner<Out, $($param),*>(
                    mut function: impl FnMut($($param),*) -> Out,
                    $($param: $param),*
                ) -> Out {
                    function($($param),*)
                }

                let ($($param,)*) = input;
                call_inner(self, $($param),*).into_system_result()
            }
        }
    };
}

impl_system_param_function!();
impl_system_param_function!(P0);
impl_system_param_function!(P0, P1);
impl_system_param_function!(P0, P1, P2);
impl_system_param_function!(P0, P1, P2, P3);
impl_system_param_function!(P0, P1, P2, P3, P4);
impl_system_param_function!(P0, P1, P2, P3, P4, P5);
impl_system_param_function!(P0, P1, P2, P3, P4, P5, P6);
impl_system_param_function!(P0, P1, P2, P3, P4, P5, P6, P7);
