use crate::app::App;

/// A unit of application extension.
///
/// Plugins are applied immediately when added and can register resources,
/// messages, and systems. The first version does not support unloading.
pub trait Plugin: Send + Sync + 'static {
    /// A diagnostic name. Defaults to the plugin's type name.
    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    /// Extends the application.
    fn build(&self, app: &mut App);
}
