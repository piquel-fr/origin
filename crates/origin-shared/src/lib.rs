//! Types shared by every origin component: the object model, the wire
//! formats, and the API contracts.

pub mod access;
pub mod api;
mod id;
pub mod object;
pub mod pack;
mod path;
pub mod refs;

pub use id::{ObjectId, ParseObjectIdError};
pub use object::{
    Commit, EncodedObject, EntryKind, EntryName, Object, ObjectKind, Signature, Tree, TreeEntry,
};
pub use path::RepoPath;
pub use refs::{BranchName, RefUpdate};
