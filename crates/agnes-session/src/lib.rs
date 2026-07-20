//! Headless session engine: NL -> DSL -> plan tree -> traced execution.
//! Emits SessionEvents to a caller-supplied EventSink. Frontends (CLI,
//! future GUI) plug in by implementing EventSink.

mod error;
mod events;
mod plan_tree;
mod session;
mod tracer_bridge;

pub use error::SessionError;
pub use events::{EventSink, NodeKindTag, SessionEvent};
pub use plan_tree::PlanTree;
pub use session::{Session, TurnInput};
