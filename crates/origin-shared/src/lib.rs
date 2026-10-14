//! Types shared by every origin component: the object model, the wire
//! formats, and the API contracts.

mod id;
pub mod object;
mod path;
pub mod refs;

pub use id::{ObjectId, ParseObjectIdError};
pub use object::{
    Commit, EncodedObject, EntryKind, EntryName, Object, ObjectKind, Signature, Tree, TreeEntry,
};
pub use path::RepoPath;
pub use refs::{BranchName, RefUpdate};
