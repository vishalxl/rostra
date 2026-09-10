use std::ops::Deref;
use std::sync::Arc;

use super::tasks::ClientTasks;

/// Unique cancellation owner, transferred from partial construction to Client.
///
/// Retired records share only completion state, never this abort-on-drop owner.
#[derive(Default)]
pub(crate) struct ClientTaskOwner {
    tasks: Arc<ClientTasks>,
    /// Inject failure after task startup but before constructor return.
    #[cfg(test)]
    pub(crate) after_start: std::sync::Mutex<Option<BuildHook>>,
}

#[cfg(test)]
pub(crate) type BuildHook =
    Box<dyn FnOnce(Arc<super::Client>) -> futures::future::BoxFuture<'static, ()> + Send>;

impl Deref for ClientTaskOwner {
    type Target = Arc<ClientTasks>;

    fn deref(&self) -> &Self::Target {
        &self.tasks
    }
}

impl Drop for ClientTaskOwner {
    fn drop(&mut self) {
        self.tasks.abort();
    }
}
