/// When deleting, whether the deletion should be only for this session or for all sessions.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DeletionScope {
    SessionOnly,
    AllSessions,
}
