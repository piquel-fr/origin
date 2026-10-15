//! Types shared by every origin component: the object model, the wire
//! formats, and the API contracts.

mod id;
mod name;
pub mod object;
mod path;
mod principal;
mod target;

pub use id::{
    AcceptanceId, BlobId, ChangeId, Digest, ObjectId, ParseIdError, RevisionId, StatementId,
    TargetId, TreeId, TypedId,
};
pub use name::InvalidName;
pub use object::{
    Acceptance, AcceptanceBody, Base, EncodedObject, EntryKind, EntryName, Inputs, Kind, NodeKey,
    NodePublicKey, Object, ObjectKind, Record, RecordId, Revision, Signature, Source, Statement,
    Tree, TreeEntry, Verdict,
};
pub use path::RepoPath;
pub use principal::{Principal, PrincipalName};
pub use target::TargetName;
