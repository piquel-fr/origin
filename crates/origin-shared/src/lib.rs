//! Types shared by every origin component: the object model, the wire
//! formats, and the API contracts.

pub mod access;
pub mod api;
pub mod event;
mod id;
pub mod landing;
mod name;
pub mod object;
pub mod pack;
mod path;
pub mod policy;
mod principal;
mod target;
mod text;

pub use id::{
    AcceptanceId, AttachmentId, BlobId, ChangeId, Digest, ObjectId, ParseIdError, RevisionId,
    StatementId, TargetId, TreeId, TypedId,
};
pub use name::InvalidName;
pub use object::{
    Acceptance, AcceptanceBody, Base, EncodedObject, EntryKind, EntryName, Inputs, Kind, NodeKey,
    NodePublicKey, Object, ObjectKind, Record, RecordId, Revision, Signature, Source, Statement,
    Tree, TreeEntry, Verdict,
};
pub use path::RepoPath;
pub use principal::{Principal, PrincipalName};
pub use target::{Head, TargetName};
pub use text::{InvalidPeriod, Period};
